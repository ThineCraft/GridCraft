use collab_core::ids::Seq;
use collab_live::client::{Crash, EntryView, Event, LiveConfig, LiveEditor, Mode, Outcome, Stamp, Step};
use collab_live::ids::{LiveId, Lookup};
use gridcraft_core::{CellRef, Value};
use gridcraft_engine::coauthor::{
    CellId, GRIDCRAFT_LIVE_FORMAT, GridCoauthor, GridLiveEditor, IdentityInsert, IdentityPosition, SharedCommand, decode_entry, mode_for,
};
use gridcraft_engine::fingerprint::{DocId, fingerprint};
use gridcraft_engine::{EngineError, Session, UiRequest};
use gridcraft_model::Cell;
use serde_json::json;
use std::sync::Arc;

use crate::live::{DeterministicServer, OrderedEntry, ProposedEntry, SimulatedServer, prepare, remote_entry, select, session_with_sheets};

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
