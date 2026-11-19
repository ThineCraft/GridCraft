use collab_core::ids::Seq;
use collab_live::client::{Crash, Effect, EntryView, Event, LiveConfig, LiveEditor, Mode, Outcome, Stamp, Step};
use collab_live::ids::{LiveId, Lookup};
use gridcraft_core::{CellRef, MAX_COLS, MAX_ROWS, Value};
use gridcraft_engine::coauthor::{
    CellId, GRIDCRAFT_LIVE_FORMAT, GridCoauthor, GridLiveEditor, IdentityInsert, IdentityPosition, LiveNotice, NotAppliedReason, SharedCommand,
    TicketAnswer, WorkbookIds, decode_entry, mode_for,
};
use gridcraft_engine::fingerprint::{DocId, fingerprint, workbook_fingerprint};
use gridcraft_engine::{DocState, EngineError, Session, Trust, UiRequest, io};
use gridcraft_model::Cell;
use protocol::live::{LiveBuild, LiveClientMsg};
use serde_json::json;
use std::sync::Arc;

use crate::live::{
    ClientPlace, DeterministicServer, OrderedEntry, ProposedEntry, SimulatedServer, prepare, remote_entry, select, session_with_sheets,
};

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

fn structural_commands() -> Vec<SharedCommand> {
    let sheet = LiveId(0, 0);
    let insert = IdentityInsert { before: IdentityPosition::Before(LiveId(0, 4)), first: LiveId(1, 0), count: 1 };
    vec![
        SharedCommand::InsertRows { sheet, rows: insert },
        SharedCommand::DeleteRows { sheet, rows: gridcraft_engine::coauthor::IdentityDelete { ids: vec![LiveId(0, 4)], first: LiveId(0, 4) } },
        SharedCommand::InsertColumns { sheet, cols: insert },
        SharedCommand::DeleteColumns { sheet, cols: gridcraft_engine::coauthor::IdentityDelete { ids: vec![LiveId(0, 4)], first: LiveId(0, 4) } },
        SharedCommand::InsertSheet {
            name: "New".into(),
            before: gridcraft_engine::coauthor::IdentitySheetInsert { before: IdentityPosition::End, id: LiveId(1, 1) },
        },
        SharedCommand::RenameSheet { sheet, name: "Renamed".into() },
        SharedCommand::DeleteSheet { sheet },
    ]
}

fn content_command() -> SharedCommand {
    let sheet = LiveId(0, 0);
    SharedCommand::CellSet {
        sheet,
        cell: CellId { sheet, row: LiveId(0, 0), col: LiveId(0, 0) },
        input: "x".into(),
        origin_year: 2026,
        formula: false,
        references: Vec::new(),
    }
}

fn session_with_stored_cells(counts: &[usize]) -> Session {
    let names = (0..counts.len()).map(|index| format!("Sheet{}", index + 1)).collect::<Vec<_>>();
    let name_refs = names.iter().map(String::as_str).collect::<Vec<_>>();
    let mut session = session_with_sheets(&name_refs);
    let document = session.doc_mut().expect("document exists");
    let workbook = Arc::make_mut(&mut document.wb);
    for (sheet_index, count) in counts.iter().copied().enumerate() {
        let sheet = Arc::make_mut(&mut workbook.sheets[sheet_index]);
        for row in 0..count {
            let row = u32::try_from(row).expect("test row fits the sheet");
            sheet.cells.set(CellRef::new(row, 0), Cell::value(Value::Number(f64::from(row))));
        }
    }
    session
}

#[test]
fn small_and_non_structural_commands_show_at_once() {
    for command in structural_commands() {
        assert_eq!(mode_for(&command, 99_999), Mode::ShowAtOnce);
    }
    assert_eq!(mode_for(&content_command(), 100_000), Mode::ShowAtOnce);
    assert_eq!(mode_for(&content_command(), usize::MAX), Mode::ShowAtOnce);

    let mut server = DeterministicServer::new();
    let mut client =
        GridCoauthor::new(live_config(), GridLiveEditor::new(session_with_stored_cells(&[50_000, 49_999])).expect("small editor builds"));
    let place = server.join(&mut client, 10);
    let proposal = submit(&mut client, &server, &place, "home.insertSheet", json!({"name":"Shown"}), 20);
    assert!(matches!(decode_entry(&proposal.payload).expect("proposal decodes").command, SharedCommand::InsertSheet { .. }));
    assert_eq!(
        client.editor().session().doc().expect("document exists").wb.sheets.len(),
        3,
        "a structural change below the threshold is shown at once"
    );
}

#[test]
fn large_structural_commands_wait_for_server() {
    for command in structural_commands() {
        assert_eq!(mode_for(&command, 100_000), Mode::WaitForServer);
        assert_eq!(mode_for(&command, usize::MAX), Mode::WaitForServer);
    }

    let mut server = DeterministicServer::new();
    let mut client =
        GridCoauthor::new(live_config(), GridLiveEditor::new(session_with_stored_cells(&[50_000, 50_000])).expect("large editor builds"));
    let place = server.join(&mut client, 10);
    let content = submit(&mut client, &server, &place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"changed"}), 20);
    assert_eq!(
        client.editor().session().doc().expect("document exists").wb.sheet(0).expect("Sheet1 exists").value(CellRef::new(0, 0)),
        Value::text("changed"),
        "content remains optimistic at the threshold"
    );
    let old_style = client
        .editor()
        .session()
        .doc()
        .expect("document exists")
        .wb
        .sheet(0)
        .expect("Sheet1 exists")
        .cells
        .get(CellRef::new(0, 0))
        .expect("A1 is stored")
        .style;
    let format = submit(&mut client, &server, &place, "home.bold", json!({"sheet":"Sheet1","range":"A1","on":true}), 21);
    let new_style = client
        .editor()
        .session()
        .doc()
        .expect("document exists")
        .wb
        .sheet(0)
        .expect("Sheet1 exists")
        .cells
        .get(CellRef::new(0, 0))
        .expect("A1 is stored")
        .style;
    assert_ne!(new_style, old_style, "formatting remains optimistic at the threshold");
    let filter = submit(&mut client, &server, &place, "data.filterSet", json!({"sheet":"Sheet1","range":"A1:A2"}), 22);
    assert!(client.editor().session().doc().expect("document exists").wb.sheet(0).expect("Sheet1 exists").autofilter.is_some());
    let structural = submit(&mut client, &server, &place, "home.insertSheet", json!({"name":"Waited"}), 23);
    assert_eq!(client.editor().session().doc().expect("document exists").wb.sheets.len(), 2, "structure waits at the threshold");

    for (offset, proposal) in [content, format, filter, structural].into_iter().enumerate() {
        let ordered = server.order(&proposal, 1_795_219_200_000 + offset as i64, 50 + offset as u32);
        server.deliver(&mut client, &ordered, 30 + offset as u64);
    }
    assert_eq!(client.editor().session().doc().expect("document exists").wb.sheets.len(), 3, "the structural change appears after acknowledgement");
}

#[test]
fn wait_for_server_acknowledgement_activates_first_local_sheet() {
    let mut server = DeterministicServer::new();
    let mut local = GridCoauthor::new(live_config(), GridLiveEditor::new(session_with_stored_cells(&[100_000])).expect("local editor builds"));
    let mut remote = GridCoauthor::new(live_config(), GridLiveEditor::new(session_with_stored_cells(&[100_000])).expect("remote editor builds"));
    let local_place = server.join(&mut local, 10);
    server.join(&mut remote, 20);
    select(remote.editor_mut().session_mut(), "Sheet1", "D4");

    let proposal = submit(&mut local, &server, &local_place, "home.insertSheet", json!({"name":"Waited"}), 30);
    let local_document = local.editor().session().doc().expect("local document exists");
    assert_eq!(local_document.wb.sheets.len(), 1, "the WaitForServer insert stays hidden before acknowledgement");
    assert_eq!(local_document.wb.active().expect("active sheet exists").name, "Sheet1");

    let ordered = server.order(&proposal, 1_795_219_200_000, 61);
    server.deliver(&mut local, &ordered, 40);
    assert_eq!(
        local.editor().session().doc().expect("local document exists").wb.active().expect("active sheet exists").name,
        "Waited",
        "the acknowledgement performs the first local application and follows its new sheet"
    );

    server.deliver(&mut remote, &ordered, 41);
    assert_eq!(
        active_view(remote.editor()),
        ("Sheet1".into(), CellRef::new(3, 3)),
        "the same entry is remote here and keeps this editor's current view"
    );
}

#[test]
fn pending_rerun_preserves_existing_and_new_sheet_views() {
    let mut server = DeterministicServer::new();
    let mut local = GridCoauthor::new(live_config(), GridLiveEditor::new(session_with_sheets(&["Data", "View"])).expect("local editor builds"));
    let mut remote = GridCoauthor::new(live_config(), GridLiveEditor::new(session_with_sheets(&["Data", "View"])).expect("remote editor builds"));
    let local_place = server.join(&mut local, 10);
    let remote_place = server.join(&mut remote, 20);

    let _pending = submit(&mut local, &server, &local_place, "home.insertSheet", json!({"name":"Pending"}), 30);
    select(local.editor_mut().session_mut(), "View", "D4");
    let first_remote = submit(&mut remote, &server, &remote_place, "cell.set", json!({"sheet":"Data","cell":"A1","input":"one"}), 31);
    let first_ordered = server.order(&first_remote, 1_795_219_200_000, 61);
    server.deliver(&mut local, &first_ordered, 40);
    assert_eq!(active_view(local.editor()), ("View".into(), CellRef::new(3, 3)), "replay keeps a view on an existing sheet");

    select(local.editor_mut().session_mut(), "Pending", "E5");
    let second_remote = submit(&mut remote, &server, &remote_place, "cell.set", json!({"sheet":"Data","cell":"A2","input":"two"}), 41);
    let second_ordered = server.order(&second_remote, 1_795_219_201_000, 62);
    server.deliver(&mut local, &second_ordered, 42);
    assert_eq!(active_view(local.editor()), ("Pending".into(), CellRef::new(4, 4)), "replay keeps a view on the pending inserted sheet");
}

fn live_config() -> LiveConfig {
    LiveConfig { format: GRIDCRAFT_LIVE_FORMAT.to_string(), crashed_at: None }
}

#[test]
fn workbook_ids_reject_inconsistent_session_data() {
    let session = session_with_sheets(&["Sheet1"]);
    let workbook = &session.doc().expect("document exists").wb;
    let ids = WorkbookIds::from_workbook(workbook).expect("base identities build");
    let valid = ids.session_data().expect("valid session data serializes");
    WorkbookIds::from_session_data(&valid, workbook).expect("valid session data loads");

    let value: serde_json::Value = serde_json::from_slice(&valid).expect("session data is JSON");
    let mut invalid = Vec::new();

    let mut missing = value.clone();
    missing["by_id"].as_array_mut().expect("sheet map is an array").clear();
    invalid.push(("missing live sheet", missing));

    let mut extra = value.clone();
    let mut entry = extra["by_id"][0].clone();
    entry["key"] = json!([9, 9]);
    entry["value"]["id"] = json!([9, 9]);
    extra["by_id"].as_array_mut().expect("sheet map is an array").push(entry);
    invalid.push(("extra live id", extra));

    let mut mismatched = value.clone();
    mismatched["by_id"][0]["value"]["id"] = json!([7, 7]);
    invalid.push(("mismatched SheetIds.id", mismatched));

    let mut short_rows = value.clone();
    short_rows["by_id"][0]["value"]["rows"]["live"][0]["len"] = json!(MAX_ROWS - 1);
    invalid.push(("short row axis", short_rows));

    let mut short_cols = value.clone();
    short_cols["by_id"][0]["value"]["cols"]["live"][0]["len"] = json!(MAX_COLS - 1);
    invalid.push(("short column axis", short_cols));

    let mut unknown = value;
    unknown["future"] = json!(true);
    invalid.push(("unknown root key", unknown));

    let mut nested_unknown: serde_json::Value = serde_json::from_slice(&valid).expect("session data is JSON");
    nested_unknown["by_id"][0]["value"]["rows"]["future"] = json!(true);
    invalid.push(("unknown nested identity key", nested_unknown));

    for (case, invalid) in invalid {
        let bytes = serde_json::to_vec(&invalid).expect("invalid probe serializes");
        assert!(WorkbookIds::from_session_data(&bytes, workbook).is_err(), "{case} must be rejected");
    }

    let oversized = vec![b' '; 8 * 1024 * 1024 + 1];
    assert!(WorkbookIds::from_session_data(&oversized, workbook).is_err(), "identity JSON over 8 MiB must be rejected before use");
}

#[test]
fn grid_host_advertises_checkpoint_capability_and_build() {
    let editor = GridLiveEditor::new(session_with_sheets(&["Sheet1"])).expect("editor builds");
    let mut client = GridCoauthor::new(live_config(), editor);
    let join = client.tick(0).into_iter().find_map(|frame| serde_json::from_str::<LiveClientMsg>(&frame).ok()).expect("GridCraft sends a join frame");
    let LiveClientMsg::LiveJoin { can_checkpoint, build, .. } = join else { panic!("first GridCraft frame must be a join") };
    assert!(can_checkpoint, "the GridCraft host can write checkpoint bytes");
    let expected = if cfg!(target_arch = "wasm32") { LiveBuild::Browser } else { LiveBuild::Desktop };
    assert_eq!(build, expected, "the host advertises its actual build kind");
}

#[test]
fn grid_checkpoint_uses_the_agreed_snapshot() {
    let mut server = DeterministicServer::new();
    let mut client = GridCoauthor::new(live_config(), GridLiveEditor::new(session_with_sheets(&["Sheet1"])).expect("editor builds"));
    server.join(&mut client, 10);

    client.submit("cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"waiting"}), 2026, 20).expect("waiting cell edit submits");
    client.submit("home.insertSheet", json!({"name":"Waiting sheet"}), 2026, 21).expect("waiting identity change submits");
    assert_eq!(value_at(&client, "A1"), Value::text("waiting"), "waiting cell edit remains shown");
    assert_eq!(client.editor().session().doc().expect("shown document exists").wb.sheets.len(), 2, "waiting sheet remains shown");

    let checkpoint = client.checkpoint(Seq(0)).expect("the agreed base checkpoints");
    let (workbook, _) = io::open_bytes("checkpoint.xlsx", &checkpoint.bytes).expect("checkpoint bytes reopen");
    assert_eq!(workbook.sheet(0).expect("Sheet1 exists").value(CellRef::new(0, 0)), Value::Empty, "waiting cell edit is absent from bytes");
    assert_eq!(workbook.sheets.len(), 1, "waiting sheet is absent from bytes");
    let stored = WorkbookIds::from_session_data(&checkpoint.session_data, &workbook).expect("checkpoint identities load");
    assert_eq!(stored.sheets.len(), 1, "waiting sheet identity is absent from session data");
    assert_eq!(checkpoint.entered, workbook_fingerprint(&workbook).entered, "entered digest covers the written agreed workbook");
}

#[test]
fn grid_checkpoint_restores_stored_view_before_writing() {
    let mut session = session_with_sheets(&["Stored", "Other"]);
    {
        let document = session.doc_mut().expect("document exists");
        let workbook = Arc::make_mut(&mut document.wb);
        workbook.set_active_sheet(0);
        let stored = Arc::make_mut(workbook.sheets.get_mut(0).expect("Stored exists"));
        stored.view_active = CellRef::new(1, 1);
        stored.view_top_left = CellRef::new(3, 2);
        stored.zoom = 125;
        stored.freeze = Some((1, 1));
        stored.show_gridlines = false;
        stored.show_headings = false;
    }
    let mut client = GridCoauthor::new(live_config(), GridLiveEditor::new(session).expect("editor builds"));
    let mut server = DeterministicServer::new();
    server.join(&mut client, 10);
    {
        let document = client.editor_mut().session_mut().doc_mut().expect("document exists");
        let workbook = Arc::make_mut(&mut document.wb);
        workbook.set_active_sheet(1);
        let personal = Arc::make_mut(workbook.sheets.get_mut(0).expect("Stored exists"));
        personal.view_active = CellRef::new(8, 8);
        personal.view_top_left = CellRef::new(9, 9);
        personal.zoom = 200;
        personal.freeze = None;
        personal.show_gridlines = true;
        personal.show_headings = true;
    }

    let checkpoint = client.checkpoint(Seq(0)).expect("checkpoint writes");
    let (written, _) = io::open_bytes("checkpoint.xlsx", &checkpoint.bytes).expect("checkpoint reopens");
    let stored = written.sheet(0).expect("Stored exists");
    assert_eq!(written.active_sheet(), 0);
    assert_eq!((stored.view_active, stored.view_top_left, stored.zoom), (CellRef::new(1, 1), CellRef::new(3, 2), 125));
    assert_eq!(stored.freeze, Some((1, 1)));
    assert!(!stored.show_gridlines && !stored.show_headings);

    let shown = &client.editor().session().doc().expect("shown document exists").wb;
    assert_eq!(shown.active_sheet(), 1, "checkpointing does not move the person");
    let personal = shown.sheet(0).expect("Stored exists");
    assert_eq!((personal.view_active, personal.view_top_left, personal.zoom), (CellRef::new(8, 8), CellRef::new(9, 9), 200));
    assert_eq!(personal.freeze, None);
    assert!(personal.show_gridlines && personal.show_headings);
}

#[test]
fn checkpoint_session_data_restores_deletion_ownership() {
    let make_session = || {
        let mut session = session_with_sheets(&["Sheet1"]);
        for (cell, input) in [("A1", "top"), ("A2", "deleted"), ("A3", "tail")] {
            session.execute("cell.set", json!({"sheet":"Sheet1","cell":cell,"input":input})).expect("base cell is set");
        }
        session
    };
    let mut server = DeterministicServer::new();
    let mut alice = GridCoauthor::new(live_config(), GridLiveEditor::new(make_session()).expect("Alice editor builds"));
    let mut bob = GridCoauthor::new(live_config(), GridLiveEditor::new(make_session()).expect("Bob editor builds"));
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);
    let deletion = submit(&mut alice, &server, &alice_place, "home.deleteRows", json!({"sheet":"Sheet1","rows":"2:2"}), 30);
    let ordered = server.order(&deletion, 1_795_219_200_000, 41);
    server.deliver_both(&mut alice, &mut bob, &ordered, 40);

    let checkpoint = bob.checkpoint(ordered.seq).expect("peer writes checkpoint");
    let (workbook, _) = io::open_bytes("checkpoint.xlsx", &checkpoint.bytes).expect("checkpoint reopens");
    let checkpoint_editor = || {
        let mut session = Session::new();
        session.add_document(DocState::from_saved(workbook.clone(), None, "checkpoint.xlsx".into(), Trust::Local));
        GridLiveEditor::from_session_data(session, &checkpoint.session_data).expect("checkpoint identities load into an editor")
    };
    let mut loaded = checkpoint_editor();
    let mut forged = checkpoint_editor();

    let undo = alice.undo(50).expect("Alice's deletion undo submits");
    let undo = server.one_proposal(&alice_place, &undo);
    loaded
        .apply(&EntryView {
            seq: Some(Seq(2)),
            participant: alice_place.participant,
            own: false,
            stamp: Some(Stamp { at_ms: 1_795_219_200_050, seed: 42 }),
            payload: &undo.payload,
        })
        .expect("loaded peer applies Alice's undo");
    assert_eq!(
        loaded.session().doc().expect("loaded document exists").wb.sheet(0).expect("Sheet1 exists").value(CellRef::new(1, 0)),
        Value::text("deleted")
    );

    let outcome = forged
        .apply(&EntryView {
            seq: Some(Seq(2)),
            participant: bob_place.participant,
            own: false,
            stamp: Some(Stamp { at_ms: 1_795_219_200_050, seed: 42 }),
            payload: &undo.payload,
        })
        .expect("forged revival is handled without a crash");
    assert!(matches!(outcome.effect, Effect::NoEffect { .. }), "another participant cannot revive Alice's deletion");
    assert_eq!(
        forged.session().doc().expect("forged document exists").wb.sheet(0).expect("Sheet1 exists").value(CellRef::new(1, 0)),
        Value::text("tail")
    );
}

fn rollback_base() -> Session {
    let mut session = session_with_sheets(&["Struct", "Filter", "RenameMe", "DeleteMe"]);
    for (cell, input) in [("B5", "40"), ("B10", "10"), ("H3", "8")] {
        session.execute("cell.set", json!({"sheet":"Struct","cell":cell,"input":input})).expect("base cell is set");
    }
    for (cell, input) in [
        ("A1", "Kind"),
        ("B1", "Amount"),
        ("C1", "Group"),
        ("A2", "x"),
        ("B2", "1"),
        ("C2", "a"),
        ("A3", "y"),
        ("B3", "2"),
        ("C3", "b"),
        ("A4", "x"),
        ("B4", "3"),
        ("C4", "a"),
    ] {
        session.execute("cell.set", json!({"sheet":"Filter","cell":cell,"input":input})).expect("filter data is set");
    }
    let document = session.doc_mut().expect("document exists");
    document.calc.fixed_now = Some(46_000.0);
    document.calc.set_rand_seed(17);
    session
}

fn rebuild_straight_through(ordered: &[OrderedEntry], pending: &[ProposedEntry]) -> (GridLiveEditor, Vec<Outcome>) {
    let mut editor = GridLiveEditor::new(rollback_base()).expect("reference editor builds");
    let mut outcomes = Vec::new();
    for entry in ordered {
        outcomes.push(
            editor
                .apply(&EntryView {
                    seq: Some(entry.seq),
                    participant: entry.participant,
                    own: false,
                    stamp: Some(Stamp { at_ms: entry.at_ms, seed: entry.seed }),
                    payload: &entry.payload,
                })
                .expect("ordered reference entry applies"),
        );
    }
    for entry in pending {
        editor
            .apply(&EntryView { seq: None, participant: entry.participant, own: true, stamp: None, payload: &entry.payload })
            .expect("pending reference entry applies");
    }
    (editor, outcomes)
}

fn assert_matches_straight_through(client: &GridCoauthor, step: &Step, ordered: &[OrderedEntry], pending: &[ProposedEntry]) {
    let (reference, outcomes) = rebuild_straight_through(ordered, pending);
    let latest = ordered.last().expect("at least one entry was delivered");
    let expected = &outcomes[outcomes.len() - 1].effect;
    assert!(step.events.iter().any(|event| matches!(event, Event::Applied { seq, effect, .. } if seq == &latest.seq && effect == expected)));
    assert_eq!(client.editor().ids(), reference.ids(), "identity tables agree after {:?}", latest.seq);
    assert_eq!(
        client.editor().session().doc().expect("shown document exists").calc.fixed_now,
        reference.session().doc().expect("reference document exists").calc.fixed_now,
        "shared clock state agrees after {:?}",
        latest.seq
    );
    let shown = fingerprint(client.editor().session(), DocId(0));
    let straight = fingerprint(reference.session(), DocId(0));
    assert_eq!(shown.entered, straight.entered, "entered fingerprint agrees after {:?}", latest.seq);
    assert_eq!(shown.computed, straight.computed, "computed fingerprint agrees after {:?}", latest.seq);
}

fn submit(
    client: &mut GridCoauthor,
    server: &DeterministicServer,
    place: &crate::live::ClientPlace,
    id: &str,
    params: serde_json::Value,
    now_ms: u64,
) -> ProposedEntry {
    let step = client.submit(id, params, 2026, now_ms).expect("shared command submits");
    server.one_proposal(place, &step)
}

#[test]
fn rollback_rerun_matches_straight_through() {
    let mut server = DeterministicServer::new();
    let mut local = GridCoauthor::new(live_config(), GridLiveEditor::new(rollback_base()).expect("local editor builds"));
    let mut remote = GridCoauthor::new(live_config(), GridLiveEditor::new(rollback_base()).expect("remote editor builds"));
    let local_place = server.join(&mut local, 10);
    let remote_place = server.join(&mut remote, 20);

    let mut pending = Vec::new();
    let commands = [
        ("cell.set", json!({"sheet":"Struct","cell":"A1","input":"ordinary"})),
        ("home.bold", json!({"sheet":"Struct","range":"A1","on":true})),
        ("home.insertRows", json!({"sheet":"Struct","rows":"5:5"})),
        ("cell.set", json!({"sheet":"Struct","cell":"C1","input":"=B6+RAND()+TODAY()"})),
        ("home.deleteRows", json!({"sheet":"Struct","rows":"12:12"})),
        ("home.insertColumns", json!({"sheet":"Struct","cols":"D:D"})),
        ("home.deleteColumns", json!({"sheet":"Struct","cols":"H:H"})),
        ("home.insertSheet", json!({"name":"LocalSheet","before":0})),
        ("sheet.rename", json!({"sheet":"RenameMe","name":"Renamed"})),
        ("home.deleteSheet", json!({"sheet":"DeleteMe"})),
        ("data.filterSet", json!({"sheet":"Filter","range":"A1:C4"})),
        ("data.filterBy", json!({"sheet":"Filter","range":"A1:C4","column":"A","values":["x"]})),
        ("data.clearFilter", json!({"sheet":"Filter","range":"A1:C4"})),
        ("data.reapply", json!({"sheet":"Filter","range":"A1:C4"})),
        ("data.filterRemove", json!({"sheet":"Filter","range":"A1:C4"})),
        ("cell.set", json!({"sheet":"Struct","cell":"D1","input":"=NOW()+RAND()"})),
    ];
    for (offset, (id, params)) in commands.into_iter().enumerate() {
        pending.push(submit(&mut local, &server, &local_place, id, params, 100 + offset as u64));
    }
    let remote_insert = submit(&mut remote, &server, &remote_place, "home.insertRows", json!({"sheet":"Struct","rows":"5:5"}), 200);

    let mut ordered = Vec::new();
    let first = server.order(&remote_insert, 1_795_219_200_000, 41);
    let step = server.deliver(&mut local, &first, 300);
    ordered.push(first);
    assert_matches_straight_through(&local, &step, &ordered, &pending);

    while !pending.is_empty() {
        let proposed = pending.remove(0);
        let next = server.order(&proposed, 1_795_219_200_000 + ordered.len() as i64 * 1_000, 41 + ordered.len() as u32);
        let step = server.deliver(&mut local, &next, 301 + ordered.len() as u64);
        ordered.push(next);
        assert_matches_straight_through(&local, &step, &ordered, &pending);
    }

    // Guarded entries: Undo takes the volatile formula away, Redo brings it back.
    for (offset, undo) in [true, false].into_iter().enumerate() {
        let step = if undo { local.undo(400 + offset as u64) } else { local.redo(400 + offset as u64) }.expect("history move submits");
        let proposed = server.one_proposal(&local_place, &step);
        let entry = server.order(&proposed, 1_795_219_300_000 + offset as i64 * 1_000, 90 + offset as u32);
        let step = server.deliver(&mut local, &entry, 410 + offset as u64);
        ordered.push(entry);
        assert_matches_straight_through(&local, &step, &ordered, &[]);
    }
}

#[test]
fn rollback_rerun_keeps_pending_identity_allocations() {
    let mut server = DeterministicServer::new();
    let mut local = GridCoauthor::new(live_config(), GridLiveEditor::new(rollback_base()).expect("local editor builds"));
    let mut remote = GridCoauthor::new(live_config(), GridLiveEditor::new(rollback_base()).expect("remote editor builds"));
    let local_place = server.join(&mut local, 10);
    let remote_place = server.join(&mut remote, 20);

    let local_insert = submit(&mut local, &server, &local_place, "home.insertRows", json!({"sheet":"Struct","rows":"5:5"}), 100);
    let formula = submit(&mut local, &server, &local_place, "cell.set", json!({"sheet":"Struct","cell":"C1","input":"=B6"}), 101);
    let remote_insert = submit(&mut remote, &server, &remote_place, "home.insertRows", json!({"sheet":"Struct","rows":"5:5"}), 102);
    let local_id = match decode_entry(&local_insert.payload).expect("local payload decodes").command {
        SharedCommand::InsertRows { rows, .. } => rows.first,
        other => panic!("expected local row insert, got {other:?}"),
    };
    let remote_id = match decode_entry(&remote_insert.payload).expect("remote payload decodes").command {
        SharedCommand::InsertRows { rows, .. } => rows.first,
        other => panic!("expected remote row insert, got {other:?}"),
    };

    let mut pending = vec![local_insert.clone(), formula.clone()];
    let mut ordered = Vec::new();
    for proposed in [&remote_insert, &local_insert, &formula] {
        if proposed.client == local_place.client {
            pending.remove(0);
        }
        let entry = server.order(proposed, 1_795_219_200_000 + ordered.len() as i64 * 1_000, 70 + ordered.len() as u32);
        let step = server.deliver(&mut local, &entry, 200 + ordered.len() as u64);
        ordered.push(entry);
        assert_matches_straight_through(&local, &step, &ordered, &pending);
    }

    let ids = local.editor().ids();
    let sheet = ids.sheet_id(0).expect("Struct has an identity");
    let rows = &ids.by_id.get(&sheet).expect("Struct axes exist").rows;
    assert_eq!(rows.find(remote_id), Lookup::At(4));
    assert_eq!(rows.find(local_id), Lookup::At(5));
    assert_eq!(rows.find(LiveId(0, 4)), Lookup::At(6));
    let value = local.editor().session().doc().expect("document exists").wb.sheet(0).expect("Struct exists").value(CellRef::new(0, 2));
    assert_eq!(value, Value::Number(40.0), "the pending formula still names the base row after both inserts");
}

fn convergence_base() -> Session {
    let mut session = session_with_sheets(&["Data", "View"]);
    for (cell, input) in [("A1", "Kind"), ("B1", "Amount"), ("A2", "base"), ("B2", "1"), ("A3", "tail"), ("B3", "2")] {
        session.execute("cell.set", json!({"sheet":"Data","cell":cell,"input":input})).expect("base data is set");
    }
    session
}

fn convergence_client() -> GridCoauthor {
    GridCoauthor::new(live_config(), GridLiveEditor::new(convergence_base()).expect("co-author editor builds"))
}

fn proposed(
    server: &DeterministicServer,
    client: &mut GridCoauthor,
    place: &crate::live::ClientPlace,
    id: &str,
    params: serde_json::Value,
    now_ms: u64,
) -> ProposedEntry {
    submit(client, server, place, id, params, now_ms)
}

fn order_for_both(
    server: &mut DeterministicServer,
    proposal: &ProposedEntry,
    first: &mut GridCoauthor,
    second: &mut GridCoauthor,
    now_ms: u64,
) -> (OrderedEntry, Step, Step) {
    let ordered = server.order(proposal, 1_795_219_200_000 + now_ms as i64, 100 + server.head().0 as u32);
    let (first_step, second_step) = server.deliver_both(first, second, &ordered, now_ms);
    (ordered, first_step, second_step)
}

fn value_at(client: &GridCoauthor, cell: &str) -> Value {
    let at = CellRef::parse(cell).expect("test cell parses");
    client.editor().session().doc().expect("document exists").wb.sheet(0).expect("Data exists").value(at)
}

fn no_effect_reason(step: &Step, seq: Seq) -> &str {
    step.events
        .iter()
        .find_map(|event| match event {
            Event::Applied { seq: event_seq, effect: Effect::NoEffect { reason }, .. } if *event_seq == seq => Some(reason.as_str()),
            _ => None,
        })
        .expect("the ordered entry reports why it had no effect")
}

fn assert_live_at(client: &mut GridCoauthor, server: &DeterministicServer, place: &crate::live::ClientPlace, head: Seq, now_ms: u64) {
    let probe = proposed(server, client, place, "cell.set", json!({"sheet":"Data","cell":"A1","input":"Kind"}), now_ms);
    assert_eq!(probe.seen, head, "a successful local submission proves Phase::Live and carries the agreed sequence");
}

#[test]
fn two_clients_converge_through_simulated_server() {
    let mut server = DeterministicServer::new();
    let mut alice = convergence_client();
    let mut bob = convergence_client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);
    select(alice.editor_mut().session_mut(), "View", "C3");
    select(bob.editor_mut().session_mut(), "Data", "F6");

    let alice_content = proposed(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Data","cell":"A2","input":"alice"}), 30);
    let bob_content = proposed(&server, &mut bob, &bob_place, "cell.set", json!({"sheet":"Data","cell":"B2","input":"=2+RAND()"}), 31);
    order_for_both(&mut server, &bob_content, &mut alice, &mut bob, 40);
    order_for_both(&mut server, &alice_content, &mut bob, &mut alice, 50);

    let alice_bold = proposed(&server, &mut alice, &alice_place, "home.bold", json!({"sheet":"Data","range":"A2","on":true}), 60);
    order_for_both(&mut server, &alice_bold, &mut alice, &mut bob, 70);
    let bob_fill = proposed(&server, &mut bob, &bob_place, "home.fillColor", json!({"sheet":"Data","range":"A2","color":"#123456"}), 80);
    order_for_both(&mut server, &bob_fill, &mut bob, &mut alice, 90);
    let filter = proposed(&server, &mut alice, &alice_place, "data.filterSet", json!({"sheet":"Data","range":"A1:B3"}), 100);
    order_for_both(&mut server, &filter, &mut bob, &mut alice, 110);
    let insert = proposed(&server, &mut bob, &bob_place, "home.insertRows", json!({"sheet":"Data","rows":"2:2"}), 120);
    order_for_both(&mut server, &insert, &mut alice, &mut bob, 130);
    let undo_step = alice.undo(140).expect("Alice's Undo submits");
    let undo = server.one_proposal(&alice_place, &undo_step);
    order_for_both(&mut server, &undo, &mut bob, &mut alice, 150);

    assert_eq!(alice.editor().ids(), bob.editor().ids());
    assert_eq!(alice.editor().agreement_digest(), bob.editor().agreement_digest());
    assert_eq!(
        fingerprint(alice.editor().session(), DocId(0)).computed,
        fingerprint(bob.editor().session(), DocId(0)).computed,
        "same-platform computed fingerprints also agree"
    );
    assert_eq!(active_view(alice.editor()), ("View".into(), CellRef::new(2, 2)));
    assert_eq!(active_view(bob.editor()), ("Data".into(), CellRef::new(5, 5)));
    let head = server.head();
    assert_live_at(&mut alice, &server, &alice_place, head, 160);
    assert_live_at(&mut bob, &server, &bob_place, head, 161);
}

#[test]
fn late_edit_to_deleted_target_reports_no_effect() {
    for (target_kind, edit, deletion_id, deletion_params) in [
        ("row", json!({"sheet":"Data","cell":"A2","input":"secret text"}), "home.deleteRows", json!({"sheet":"Data","rows":"2:2"})),
        ("column", json!({"sheet":"Data","cell":"B1","input":"secret text"}), "home.deleteColumns", json!({"sheet":"Data","cols":"B:B"})),
        ("sheet", json!({"sheet":"Data","cell":"A1","input":"secret text"}), "home.deleteSheet", json!({"sheet":"Data"})),
    ] {
        let mut server = DeterministicServer::new();
        let mut alice = convergence_client();
        let mut bob = convergence_client();
        let alice_place = server.join(&mut alice, 10);
        let bob_place = server.join(&mut bob, 20);

        let late = proposed(&server, &mut alice, &alice_place, "cell.set", edit, 30);
        let deletion = proposed(&server, &mut bob, &bob_place, deletion_id, deletion_params, 31);
        order_for_both(&mut server, &deletion, &mut alice, &mut bob, 40);
        let alice_after_deletion = alice.editor().agreement_digest();
        let bob_after_deletion = bob.editor().agreement_digest();
        let (ordered, alice_step, bob_step) = order_for_both(&mut server, &late, &mut bob, &mut alice, 50);

        for step in [&alice_step, &bob_step] {
            let reason = no_effect_reason(step, ordered.seq);
            assert!(reason.contains("cell.set"), "the action is named: {reason}");
            assert!(reason.to_ascii_lowercase().contains(target_kind), "the deleted target kind is named: {reason}");
            assert!(!reason.contains("secret text"), "cell content is not exposed: {reason}");
        }
        assert_eq!(alice.editor().agreement_digest(), alice_after_deletion, "late edit to deleted {target_kind} changes no entered state");
        assert_eq!(bob.editor().agreement_digest(), bob_after_deletion, "late edit to deleted {target_kind} changes no entered state");
        assert_eq!(alice.editor().agreement_digest(), bob.editor().agreement_digest());
    }
}

#[test]
fn same_cell_later_log_entry_wins() {
    let mut server = DeterministicServer::new();
    let mut alice = convergence_client();
    let mut bob = convergence_client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);
    let alice_edit = proposed(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Data","cell":"A2","input":"alice"}), 30);
    let bob_edit = proposed(&server, &mut bob, &bob_place, "cell.set", json!({"sheet":"Data","cell":"A2","input":"bob"}), 31);

    order_for_both(&mut server, &alice_edit, &mut bob, &mut alice, 40);
    order_for_both(&mut server, &bob_edit, &mut alice, &mut bob, 50);

    assert_eq!(value_at(&alice, "A2"), Value::text("bob"));
    assert_eq!(value_at(&bob, "A2"), Value::text("bob"));
    assert_eq!(alice.editor().agreement_digest(), bob.editor().agreement_digest());
}

#[test]
fn concurrent_inserts_before_one_row_keep_both_blocks() {
    let mut server = DeterministicServer::new();
    let mut alice = convergence_client();
    let mut bob = convergence_client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);
    let alice_insert = proposed(&server, &mut alice, &alice_place, "home.insertRows", json!({"sheet":"Data","rows":"2:3"}), 30);
    let bob_insert = proposed(&server, &mut bob, &bob_place, "home.insertRows", json!({"sheet":"Data","rows":"2:3"}), 31);
    let alice_first = match decode_entry(&alice_insert.payload).expect("Alice's insert decodes").command {
        SharedCommand::InsertRows { rows, .. } => rows.first,
        other => panic!("expected row insert, got {other:?}"),
    };
    let bob_first = match decode_entry(&bob_insert.payload).expect("Bob's insert decodes").command {
        SharedCommand::InsertRows { rows, .. } => rows.first,
        other => panic!("expected row insert, got {other:?}"),
    };

    order_for_both(&mut server, &alice_insert, &mut bob, &mut alice, 40);
    order_for_both(&mut server, &bob_insert, &mut alice, &mut bob, 50);

    for client in [&alice, &bob] {
        let ids = client.editor().ids();
        let sheet = ids.sheet_id(0).expect("Data has an identity");
        let rows = &ids.by_id.get(&sheet).expect("Data axes exist").rows;
        assert_eq!(rows.find(alice_first), Lookup::At(1));
        assert_eq!(rows.find(LiveId(alice_first.0, alice_first.1 + 1)), Lookup::At(2));
        assert_eq!(rows.find(bob_first), Lookup::At(3));
        assert_eq!(rows.find(LiveId(bob_first.0, bob_first.1 + 1)), Lookup::At(4));
        assert_eq!(rows.find(LiveId(0, 1)), Lookup::At(5));
    }
    assert_eq!(alice.editor().agreement_digest(), bob.editor().agreement_digest());
}

#[test]
fn undo_never_removes_the_other_participants_work() {
    let mut server = DeterministicServer::new();
    let mut alice = convergence_client();
    let mut bob = convergence_client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);
    let alice_edit = proposed(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Data","cell":"A2","input":"alice"}), 30);
    order_for_both(&mut server, &alice_edit, &mut alice, &mut bob, 40);
    let bob_edit = proposed(&server, &mut bob, &bob_place, "cell.set", json!({"sheet":"Data","cell":"A2","input":"bob"}), 50);
    order_for_both(&mut server, &bob_edit, &mut bob, &mut alice, 60);
    let undo_step = alice.undo(70).expect("Alice's Undo submits");
    let undo = server.one_proposal(&alice_place, &undo_step);
    let (ordered, alice_step, bob_step) = order_for_both(&mut server, &undo, &mut bob, &mut alice, 80);

    for step in [&alice_step, &bob_step] {
        let reason = no_effect_reason(step, ordered.seq);
        assert!(reason.contains("Undo"), "the action is named: {reason}");
        assert!(reason.to_ascii_lowercase().contains("cell"), "the guarded target kind is named: {reason}");
        assert!(!reason.contains("alice") && !reason.contains("bob"), "cell content is not exposed: {reason}");
    }
    assert_eq!(value_at(&alice, "A2"), Value::text("bob"));
    assert_eq!(value_at(&bob, "A2"), Value::text("bob"));
}

#[test]
fn entry_cannot_allocate_another_participants_identities() {
    let mut server = DeterministicServer::new();
    let mut alice = convergence_client();
    let mut bob = convergence_client();
    let mut carol = convergence_client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);
    server.join(&mut carol, 25);
    assert_ne!(alice_place.participant, bob_place.participant);

    // Bob's client is hostile: it names Alice's next identities as its new rows. Alice and Carol
    // are honest and receive it.
    let honest = proposed(&server, &mut bob, &bob_place, "home.insertRows", json!({"sheet":"Data","rows":"2:2"}), 30);
    let mut forged_payload: serde_json::Value = serde_json::from_str(&honest.payload).expect("payload is JSON");
    forged_payload["command"]["rows"]["first"] = serde_json::to_value(LiveId(alice_place.participant.0, 0)).expect("identity serializes");
    forged_payload["command"]["rows"]["count"] = json!(1_000);
    let forged = ProposedEntry { payload: forged_payload.to_string(), ..honest };

    let alice_insert = proposed(&server, &mut alice, &alice_place, "home.insertRows", json!({"sheet":"Data","rows":"3:3"}), 40);
    let alice_first = match decode_entry(&alice_insert.payload).expect("Alice's payload decodes").command {
        SharedCommand::InsertRows { rows, .. } => rows.first,
        other => panic!("expected a row insert, got {other:?}"),
    };
    assert!(alice_first.1 < 1_000, "the forged block covers Alice's next identity");

    let (forged_entry, alice_step, carol_step) = order_for_both(&mut server, &forged, &mut alice, &mut carol, 50);
    for step in [&alice_step, &carol_step] {
        assert_eq!(no_effect_reason(step, forged_entry.seq), "The shared change is not valid.");
    }
    let (alice_entry, alice_step, carol_step) = order_for_both(&mut server, &alice_insert, &mut alice, &mut carol, 60);
    for step in [&alice_step, &carol_step] {
        assert!(
            step.events.iter().any(|event| matches!(event, Event::Applied { seq, effect: Effect::Applied, .. } if *seq == alice_entry.seq)),
            "Alice's own insert still applies: {:?}",
            step.events
        );
    }
    assert_eq!(value_at(&alice, "A2"), Value::text("base"), "the forged rows were not inserted");
    assert_eq!(value_at(&alice, "A3"), Value::Empty, "Alice's row was inserted");
    assert_eq!(value_at(&alice, "A4"), Value::text("tail"));
    assert_eq!(alice.editor().ids(), carol.editor().ids(), "identity tables agree");
    assert_eq!(alice.editor().agreement_digest(), carol.editor().agreement_digest(), "entered content agrees");
}

#[test]
fn every_identity_allocating_command_must_use_the_senders_identities() {
    let commands = [
        ("home.insertRows", json!({"sheet":"Data","rows":"2:2"})),
        ("home.deleteRows", json!({"sheet":"Data","rows":"2:2"})),
        ("home.insertColumns", json!({"sheet":"Data","cols":"B:B"})),
        ("home.deleteColumns", json!({"sheet":"Data","cols":"B:B"})),
        ("home.insertSheet", json!({"name":"Squatted"})),
    ];
    for (id, params) in commands {
        let mut editor = GridLiveEditor::new(convergence_base()).expect("editor builds");
        let before_ids = editor.ids().clone();
        let before = fingerprint(editor.session(), DocId(0)).entered;
        let mut next = 0u32;
        // Participant 1's identities, in an entry that participant 2 sends.
        let change = editor
            .prepare(1, id, &params, 2026, &mut |count| {
                let first = LiveId(1, next);
                next = next.checked_add(count).ok_or(collab_live::ids::IdError::NoPlace)?;
                Ok(first)
            })
            .expect("change prepares");
        let outcome = editor.apply(&remote_entry(&change.payload, 1)).expect("entry is handled");
        assert_eq!(outcome.effect, Effect::NoEffect { reason: "The shared change is not valid.".into() }, "{id}");
        assert_eq!(editor.ids(), &before_ids, "{id} leaves the identity tables alone");
        assert_eq!(fingerprint(editor.session(), DocId(0)).entered, before, "{id} leaves the workbook alone");

        let own = prepare(&mut editor, 2, id, params.clone(), &mut next);
        let outcome = editor.apply(&remote_entry(&own.payload, 2)).expect("entry is handled");
        assert_eq!(outcome.effect, Effect::Applied, "{id} with the sender's own identities applies");
    }
}

fn protected_budget_session() -> Session {
    let mut session = session_with_sheets(&["Data"]);
    session
        .execute("review.allowEditRanges", json!({"sheet":"Data","action":"add","title":"Budget","range":"B1:B3","password":"pa55"}))
        .expect("password range is added");
    session.execute("review.protectSheet", json!({"sheet":"Data"})).expect("sheet is protected");
    session
}

fn session_on(workbook: &gridcraft_model::Workbook) -> Session {
    let mut session = Session::new();
    session.add_document(gridcraft_engine::DocState::new(workbook.clone(), None, "Book1".into(), gridcraft_engine::Trust::Local));
    session
}

#[test]
fn a_locally_unlocked_edit_range_does_not_split_shared_content() {
    let base = protected_budget_session();
    let workbook = base.doc().expect("document exists").wb.as_ref().clone();
    let mut alice_session = session_on(&workbook);
    alice_session
        .execute("review.allowEditRanges", json!({"sheet":"Data","action":"unlock","title":"Budget","password":"pa55"}))
        .expect("Alice unlocks the range on her own client");
    let mut alice = GridLiveEditor::new(alice_session).expect("Alice's editor builds");
    let mut bob = GridLiveEditor::new(session_on(&workbook)).expect("Bob's editor builds");

    let mut next = 0;
    let change = prepare(&mut alice, 1, "cell.set", json!({"sheet":"Data","cell":"B1","input":"alice"}), &mut next);
    let entry = remote_entry(&change.payload, 1);
    let on_alice = alice.apply(&EntryView { own: true, ..entry }).expect("Alice handles her entry");
    let on_bob = bob.apply(&remote_entry(&change.payload, 1)).expect("Bob handles Alice's entry");

    assert_eq!(on_alice.effect, on_bob.effect, "every client gives the ordered entry the same outcome");
    assert!(matches!(on_bob.effect, Effect::NoEffect { .. }), "a password range stays locked for shared changes");
    assert_eq!(alice.agreement_digest(), bob.agreement_digest(), "entered content agrees");
}

fn debuggable_session() -> Session {
    let mut session = session_with_sheets(&["Data"]);
    let script = json!({"name":"M","output":{"onConflict":"overwrite"},"steps":[
        {"command":"cell.set","params":{"cell":"C1","input":"x"}},
        {"command":"cell.set","params":{"cell":"C2","input":"y"}}
    ]});
    let shared_macro = gridcraft_engine::macros::parse_macro(&script).expect("macro parses");
    let document = session.doc_mut().expect("document exists");
    Arc::make_mut(&mut document.wb).macros.push(shared_macro);
    session
}

#[test]
fn the_macro_debugger_cannot_lock_a_shared_workbook() {
    let mut alice = GridLiveEditor::new(debuggable_session()).expect("Alice's editor builds");
    let mut bob = GridLiveEditor::new(debuggable_session()).expect("Bob's editor builds");
    let started = alice.session_mut().execute("macro.debug", json!({"name":"M"}));
    assert!(started.is_err(), "a macro cannot be debugged on a shared workbook: {started:?}");

    let mut next = 0;
    let change = prepare(&mut bob, 1, "cell.set", json!({"sheet":"Data","cell":"A1","input":"bob"}), &mut next);
    let on_alice = alice.apply(&remote_entry(&change.payload, 1)).expect("Alice handles Bob's entry");
    let on_bob = bob.apply(&remote_entry(&change.payload, 1)).expect("Bob handles his entry");
    assert_eq!(on_alice.effect, Effect::Applied);
    assert_eq!(on_bob.effect, Effect::Applied);
    assert_eq!(alice.agreement_digest(), bob.agreement_digest(), "entered content agrees");

    let mut debugging = debuggable_session();
    debugging.execute("macro.debug", json!({"name":"M"})).expect("debugging starts before sharing");
    assert!(GridLiveEditor::new(debugging).is_err(), "a workbook being debugged cannot start sharing");
}

/// The same client joins again after its place ended (C22): a new participant number, its
/// waiting batch held until the room's version before it is stored, then sent.
fn rejoin_returning(client: &mut GridCoauthor, previous: u32, participant: u32, head: u64, now_ms: u64) -> u64 {
    let join = client.connected(now_ms);
    let join: serde_json::Value = serde_json::from_str(&join[0]).expect("join is JSON");
    assert!(join["returning_through"].is_number(), "the join names the end of the waiting batch: {join}");
    let joined = json!({
        "type":"live_joined",
        "session":"00000000-0000-0000-0000-00000000005e",
        "participant":participant,
        "place":{"is":"returning","previous":previous},
        "can_edit":true,
        "heartbeat_ms":60_000,
        "people":[]
    });
    client.receive(&joined.to_string(), now_ms + 1).expect("the returning join is accepted");
    client.receive(&json!({"type":"catchup","base":0,"revision":null,"from":0,"head":head}).to_string(), now_ms + 2).expect("catch-up is accepted");
    join["returning_through"].as_u64().expect("a local number")
}

/// A returning person's waiting cell edit lands on a row someone else deleted while they were
/// away: it does nothing on every machine, and its author is told, with the text they typed.
#[test]
fn returning_deleted_target_is_no_effect_and_kept_for_the_author() {
    let mut server = DeterministicServer::new();
    let mut alice = GridCoauthor::new(live_config(), GridLiveEditor::new(session_with_sheets(&["Data"])).expect("editor builds"));
    let mut bob = GridCoauthor::new(live_config(), GridLiveEditor::new(session_with_sheets(&["Data"])).expect("editor builds"));
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 10);
    let _ = alice.take_notices();

    // Alice types into B5; the frame never reaches the room before her connection drops.
    let lost = alice.submit("cell.set", json!({"sheet":0,"cell":"B5","input":"kept text"}), 2026, 20).expect("the edit is made");
    assert_eq!(server.proposals(&alice_place, &lost).len(), 1);
    alice.disconnected(30);

    // Meanwhile Bob deletes row 5, and Alice's place ends.
    let delete = server.one_proposal(&bob_place, &bob.submit("home.deleteRows", json!({"sheet":0,"rows":"5:5"}), 2026, 40).expect("delete"));
    let ordered = server.order(&delete, 1_795_219_200_000, 3);
    server.deliver(&mut bob, &ordered, 41);

    let through = rejoin_returning(&mut alice, alice_place.participant.0, 7, 1, 100);
    server.deliver(&mut alice, &ordered, 103);
    let ready = alice.receive(&json!({"type":"return_ready","through":through}).to_string(), 104).expect("the batch may go");
    let returned = ClientPlace { client: alice_place.client, participant: protocol::live::ParticipantNo(7) };
    let batch = server.one_proposal(&returned, &ready);
    let stored = server.order(&batch, 1_795_219_201_000, 4);
    let (alice_step, _) = server.deliver_both(&mut alice, &mut bob, &stored, 110);

    assert!(
        alice_step.events.iter().any(|event| matches!(event, Event::Applied { own: true, effect: Effect::NoEffect { .. }, .. })),
        "the returning change does nothing: {alice_step:?}"
    );
    assert_eq!(fingerprint(alice.editor().session(), DocId(0)).entered, fingerprint(bob.editor().session(), DocId(0)).entered, "both copies agree");
    let notices = alice.take_notices();
    let kept = notices.iter().find_map(|notice| match notice {
        LiveNotice::NotApplied { kept_text, reason, .. } => Some((kept_text.clone(), *reason)),
        _ => None,
    });
    assert_eq!(kept, Some((Some("kept text".to_string()), NotAppliedReason::RowDeleted)), "the author keeps what they typed: {notices:?}");
}

/// A return older than the retention: the room asks the returning editor for its old shown copy
/// (`ArchiveReturn`, then an archive ticket) and stores it below the current file. The copy is
/// what the person saw before the current base loaded, written as deterministic bytes.
#[test]
fn archive_return_writes_the_old_shown_copy() {
    let mut server = DeterministicServer::new();
    let mut alice = GridCoauthor::new(live_config(), GridLiveEditor::new(session_with_sheets(&["Data"])).expect("editor builds"));
    let place = server.join(&mut alice, 10);
    let _ = alice.submit("cell.set", json!({"sheet":0,"cell":"A1","input":"old copy"}), 2026, 20).expect("the edit is made");
    alice.disconnected(30);

    let join = alice.connected(100);
    assert!(join[0].contains("returning_through"));
    let joined = json!({"type":"live_joined","session":"00000000-0000-0000-0000-00000000005e","participant":8,
        "place":{"is":"returning","previous":place.participant.0},"can_edit":true,"heartbeat_ms":60_000,"people":[]});
    alice.receive(&joined.to_string(), 101).expect("join");
    let archive = alice.receive(&json!({"type":"archive_return","current":"r9"}).to_string(), 102).expect("archive return");
    assert!(archive.events.iter().any(|event| matches!(event, Event::ArchiveReturn { .. })));
    let catchup = alice.receive(&json!({"type":"catchup","base":4,"revision":"r9","from":4,"head":4}).to_string(), 103).expect("catch-up");
    assert!(catchup.events.iter().any(|event| matches!(event, Event::LoadBase { .. })));
    alice.load_base(gridcraft_engine::coauthor::LiveBase::Empty).expect("the current base loads");
    alice.base_ready(104).expect("live on the current base");

    let ticket = json!({"type":"checkpoint_ticket","ticket":"00000000-0000-0000-0000-0000000000b1","purpose":"write","at":4,"base":"r9","major":"returning_client_archive"});
    let step = alice.receive(&ticket.to_string(), 105).expect("ticket");
    let event = step.events.iter().find(|event| matches!(event, Event::CheckpointRequested { .. })).expect("an archive ticket");
    let Some(TicketAnswer::Write { checkpoint, .. }) = alice.answer_ticket(event) else { panic!("the archive ticket is answered") };
    assert!(checkpoint.session_data.is_empty(), "the old copy is never a base: no identities");
    let (workbook, _) = io::open_bytes("archive.xlsx", &checkpoint.bytes).expect("the copy opens");
    let a1 = workbook.sheet(0).expect("sheet").value(CellRef::parse("A1").expect("cell"));
    assert_eq!(a1, Value::Text("old copy".into()), "the copy is what the person saw, waiting change included");
    assert_eq!(checkpoint.bytes, archived_copy("old copy"), "another editor with the same changes writes the same bytes");

    // An ordinary ticket still writes the agreed state.
    let ordinary =
        json!({"type":"checkpoint_ticket","ticket":"00000000-0000-0000-0000-0000000000b2","purpose":"write","at":4,"base":"r9","major":null});
    let step = alice.receive(&ordinary.to_string(), 106).expect("ticket");
    let event = step.events.iter().find(|event| matches!(event, Event::CheckpointRequested { .. })).expect("a write ticket");
    let Some(TicketAnswer::Write { checkpoint, .. }) = alice.answer_ticket(event) else { panic!("answered") };
    let (workbook, _) = io::open_bytes("current.xlsx", &checkpoint.bytes).expect("opens");
    assert_eq!(workbook.sheet(0).expect("sheet").value(CellRef::parse("A1").expect("cell")), Value::Empty, "the current state");
}

#[test]
fn an_over_limit_room_is_a_recoverable_pause() {
    let mut server = DeterministicServer::new();
    let mut alice = GridCoauthor::new(live_config(), GridLiveEditor::new(session_with_sheets(&["Data"])).expect("editor builds"));
    server.join(&mut alice, 10);
    let step = alice.receive(&json!({"type":"unavailable","reason":"log_over_limit"}).to_string(), 20).expect("not a fault");
    assert!(step.events.iter().any(|event| matches!(event, Event::Unavailable { .. })));
    assert_eq!(alice.phase(), gridcraft_engine::coauthor::Phase::Offline, "it waits to join again");
    assert!(!alice.connected(30).is_empty(), "and can join again");
}

/// The archived copy an independent editor writes after typing `text` into A1 and being archived
/// on its return.
fn archived_copy(text: &str) -> Vec<u8> {
    let mut server = DeterministicServer::new();
    let mut editor = GridCoauthor::new(live_config(), GridLiveEditor::new(session_with_sheets(&["Data"])).expect("editor builds"));
    let place = server.join(&mut editor, 10);
    let _ = editor.submit("cell.set", json!({"sheet":0,"cell":"A1","input":text}), 2026, 20).expect("the edit is made");
    editor.disconnected(30);
    let _ = editor.connected(100);
    let joined = json!({"type":"live_joined","session":"00000000-0000-0000-0000-00000000005e","participant":9,
        "place":{"is":"returning","previous":place.participant.0},"can_edit":true,"heartbeat_ms":60_000,"people":[]});
    editor.receive(&joined.to_string(), 101).expect("join");
    editor.receive(&json!({"type":"archive_return","current":"r9"}).to_string(), 102).expect("archive return");
    let ticket = json!({"type":"checkpoint_ticket","ticket":"00000000-0000-0000-0000-0000000000c1","purpose":"write","at":0,"base":"r9","major":"returning_client_archive"});
    let step = editor.receive(&ticket.to_string(), 103).expect("a ticket while joining");
    let event = step.events.iter().find(|event| matches!(event, Event::CheckpointRequested { .. })).expect("the archive ticket");
    match editor.answer_ticket(event) {
        Some(TicketAnswer::Write { checkpoint, .. }) => checkpoint.bytes,
        _ => panic!("the archive ticket is answered"),
    }
}
