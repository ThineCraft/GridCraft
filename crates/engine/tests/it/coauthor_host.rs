//! The engine's host surface for a live document: routing, explicit targets, start and finish,
//! presence and notices (plan G5, task 9).

use collab_core::ids::Seq;
use collab_live::client::{Event, LiveConfig, LocalError, Step};
use collab_live::ids::LiveId;
use gridcraft_core::{CellRef, RangeRef};
use gridcraft_engine::coauthor::{
    GRIDCRAFT_LIVE_FORMAT, GridCoauthor, GridLiveEditor, GridLiveError, LiveBase, LiveNotice, MAX_PRESENCE_BYTES, NotAppliedReason, Phase, Route,
    ShownPresence, UNSHARED_REASON, explicit_params, is_local_command, is_shared_command, route,
};
use gridcraft_engine::fingerprint::{DocId, fingerprint};
use gridcraft_engine::{Session, command_specs, io};
use protocol::live::LiveClientMsg;
use serde_json::{Value as Json, json};
use std::sync::Arc;

use crate::live::{ClientPlace, DeterministicServer, ProposedEntry, select, session_with_sheets};

const ORIGIN_YEAR: i32 = 2026;

fn live_config() -> LiveConfig {
    LiveConfig { format: GRIDCRAFT_LIVE_FORMAT.to_string(), crashed_at: None }
}

fn coauthor(names: &[&str]) -> GridCoauthor {
    GridCoauthor::new(live_config(), GridLiveEditor::new(session_with_sheets(names)).expect("editor builds"))
}

/// A co-author joined to its own test room, so it is live.
fn live_coauthor(names: &[&str]) -> GridCoauthor {
    let mut client = coauthor(names);
    DeterministicServer::new().join(&mut client, 1);
    client
}

/// The same client joins again on a new connection and keeps its place.
fn rejoin(client: &mut GridCoauthor, now_ms: u64) {
    let _ = client.connected(now_ms);
    let joined = json!({
        "type":"live_joined",
        "session":"00000000-0000-0000-0000-00000000005e",
        "participant":1,
        "place":{"is":"kept"},
        "can_edit":true,
        "heartbeat_ms":60_000,
        "people":[]
    });
    client.receive(&joined.to_string(), now_ms + 1).expect("join is accepted");
    let step = client.receive(&json!({"type":"catchup","base":0,"revision":null,"from":0,"head":0}).to_string(), now_ms + 2).expect("catch-up");
    if step.events.iter().any(|event| matches!(event, Event::LoadBase { .. })) {
        client.base_ready(now_ms + 3).expect("base");
    }
}

fn submit(client: &mut GridCoauthor, server: &DeterministicServer, place: &ClientPlace, id: &str, params: Json, now_ms: u64) -> ProposedEntry {
    let step = client.submit(id, params, ORIGIN_YEAR, now_ms).expect("shared change is accepted locally");
    server.one_proposal(place, &step)
}

/// Joins `client` as participant `participant` with the given right to edit, without the shared
/// test server's always-editing join.
fn join_with_access(client: &mut GridCoauthor, participant: u32, can_edit: bool, now_ms: u64) {
    let join = client.tick(now_ms).into_iter().any(|frame| matches!(serde_json::from_str(&frame), Ok(LiveClientMsg::LiveJoin { .. })));
    assert!(join, "a new co-author sends a live join");
    client
        .receive(
            &json!({
                "type":"live_joined",
                "session":"00000000-0000-0000-0000-00000000005e",
                "participant":participant,
                "place":{"is":"new"},
                "can_edit":can_edit,
                "heartbeat_ms":60_000,
                "people":[]
            })
            .to_string(),
            now_ms.saturating_add(1),
        )
        .expect("join is accepted");
    client
        .receive(&json!({"type":"catchup","base":0,"revision":null,"from":0,"head":0}).to_string(), now_ms.saturating_add(2))
        .expect("catch-up is accepted");
    client.base_ready(now_ms.saturating_add(3)).expect("loaded base is accepted");
}

/// Joins `client` as the room does since ShareCraft's G4 final review: the pause state
/// (`changes_paused` or `changes_resumed`) arrives between `live_joined` and `catchup`.
fn join_told_pause_state(client: &mut GridCoauthor, pause_state: &Json, now_ms: u64) {
    let join = client.tick(now_ms).into_iter().any(|frame| matches!(serde_json::from_str(&frame), Ok(LiveClientMsg::LiveJoin { .. })));
    assert!(join, "a new co-author sends a live join");
    let joined = json!({"type":"live_joined","session":"00000000-0000-0000-0000-00000000005e","participant":1,
        "place":{"is":"new"},"can_edit":true,"heartbeat_ms":60_000,"people":[]});
    client.receive(&joined.to_string(), now_ms.saturating_add(1)).expect("join is accepted");
    client.receive(&pause_state.to_string(), now_ms.saturating_add(2)).expect("the pause state is accepted before the catch-up");
    let step = client
        .receive(&json!({"type":"catchup","base":0,"revision":null,"from":0,"head":0}).to_string(), now_ms.saturating_add(3))
        .expect("catch-up is accepted after the pause state");
    assert!(step.events.iter().any(|event| matches!(event, Event::LoadBase { .. })));
    client.base_ready(now_ms.saturating_add(4)).expect("loaded base is accepted");
    assert_eq!(client.phase(), Phase::Live, "the join ends live");
}

fn sends_entry(frames: &[String]) -> bool {
    frames.iter().any(|frame| matches!(serde_json::from_str(frame), Ok(LiveClientMsg::Entry { .. })))
}

/// Every join is told whether changes wait before its catch-up. A join that is not paused shows
/// nothing and sends changes at once; a paused join is told so, holds the person's change and
/// sends it when changes resume.
#[test]
fn a_join_is_told_whether_changes_wait_before_its_catch_up() {
    let mut open = coauthor(&["Data"]);
    join_told_pause_state(&mut open, &json!({"type":"changes_resumed"}), 10);
    assert!(open.take_notices().is_empty(), "a join that was never paused is not told changes resumed");
    let step = open.submit("cell.set", json!({"sheet":0,"cell":"A1","input":"1"}), ORIGIN_YEAR, 20).expect("the edit is made");
    assert!(sends_entry(&step.send), "an open room takes the change at once");

    let mut paused = coauthor(&["Data"]);
    join_told_pause_state(&mut paused, &json!({"type":"changes_paused","entries":5_000,"bytes":1_024}), 10);
    assert_eq!(paused.take_notices(), vec![LiveNotice::Paused { entries: 5_000, bytes: 1_024 }]);
    let step = paused.submit("cell.set", json!({"sheet":0,"cell":"A1","input":"1"}), ORIGIN_YEAR, 20).expect("the edit is made");
    assert!(!sends_entry(&step.send) && !sends_entry(&paused.tick(30)), "a paused room holds the change");
    assert_eq!(paused.waiting(), 1, "the held change still waits");
    let step = paused.receive(&json!({"type":"changes_resumed"}).to_string(), 40).expect("the resume is accepted");
    assert!(sends_entry(&step.send), "the held change goes when changes resume");
    assert_eq!(paused.take_notices(), vec![LiveNotice::Resumed], "and the person is told");
}

/// A pause the person was already told about is not told again (a rejoin while paused hears it
/// once more before its catch-up).
#[test]
fn a_repeated_pause_is_told_once() {
    let mut paused = coauthor(&["Data"]);
    join_told_pause_state(&mut paused, &json!({"type":"changes_paused","entries":5_000,"bytes":1_024}), 10);
    assert_eq!(paused.take_notices(), vec![LiveNotice::Paused { entries: 5_000, bytes: 1_024 }]);
    paused.receive(&json!({"type":"changes_paused","entries":5_001,"bytes":1_100}).to_string(), 20).expect("still paused");
    assert!(paused.take_notices().is_empty(), "the person already knows changes wait");
    paused.receive(&json!({"type":"changes_resumed"}).to_string(), 30).expect("resumed");
    paused.receive(&json!({"type":"changes_resumed"}).to_string(), 40).expect("resumed again");
    assert_eq!(paused.take_notices(), vec![LiveNotice::Resumed], "one resume is told once");
}

fn entered(client: &GridCoauthor) -> [u8; 32] {
    fingerprint(client.editor().session(), DocId(0)).entered
}

fn active_cell(client: &GridCoauthor) -> (usize, CellRef) {
    let document = client.editor().session().doc().expect("document exists");
    (document.wb.active_sheet(), document.selection.active)
}

fn debuggable_session() -> Session {
    let mut session = session_with_sheets(&["Data"]);
    let script = json!({"name":"M","output":{"onConflict":"overwrite"},"steps":[
        {"command":"cell.set","params":{"cell":"C1","input":"x"}}
    ]});
    let shared_macro = gridcraft_engine::macros::parse_macro(&script).expect("macro parses");
    let document = session.doc_mut().expect("document exists");
    Arc::make_mut(&mut document.wb).macros.push(shared_macro);
    session
}

#[test]
fn leaving_the_room_clears_coauthoring() {
    let mut client = coauthor(&["Data"]);
    assert!(client.editor().session().is_coauthoring(), "a live editor marks its session as shared");
    let mut server = DeterministicServer::new();
    server.join(&mut client, 10);

    let leave = client.leave();
    assert!(leave.is_some_and(|frame| matches!(serde_json::from_str(&frame), Ok(LiveClientMsg::Leave))), "leaving says so to the room");
    let session = client.finish();

    assert!(!session.is_coauthoring(), "the session handed back is no longer shared");
    assert!(session.doc().is_ok(), "the workbook stays open");
}

#[test]
fn macro_debugger_can_start_after_leaving() {
    let mut client = GridCoauthor::new(live_config(), GridLiveEditor::new(debuggable_session()).expect("editor builds"));
    let refused = client.editor_mut().session_mut().execute("macro.debug", json!({"name":"M"}));
    assert!(refused.is_err(), "the debugger stays off while shared: {refused:?}");

    let mut session = client.finish();

    session.execute("macro.debug", json!({"name":"M"})).expect("the debugger starts once the workbook is no longer shared");
}

#[test]
fn every_shared_command_routes_shared_and_view_commands_route_local() {
    for id in ["cell.set", "home.bold", "home.insertRows", "home.deleteSheet", "data.filterBy", "sheet.rename"] {
        assert_eq!(route(id), Route::Shared, "{id}");
    }
    for id in [
        "selection.set",
        "selection.move",
        "sheet.activate",
        "sheet.next",
        "sheet.previous",
        "view.zoom",
        "view.freezePanes",
        "view.unfreezePanes",
        "view.gridlines",
        "edit.copy",
        "edit.find",
        "cell.get",
    ] {
        assert_eq!(route(id), Route::Local, "{id}");
    }
    for id in ["edit.replace", "edit.cut", "edit.paste", "home.mergeCells", "data.sortAscending", "macro.run", "file.save"] {
        assert_eq!(route(id), Route::Refused(UNSHARED_REASON), "{id}");
    }

    // Writes kept in the workbook are never personal, even when the command is not journaled.
    for id in ["formulas.watchWindow", "macro.check"] {
        assert_eq!(route(id), Route::Refused(UNSHARED_REASON), "{id}");
    }

    // Both lists name real commands; what personal commands do is pinned by
    // `local_commands_leave_the_shared_workbook_unchanged`.
    let registered: Vec<&str> = command_specs().iter().map(|spec| spec.id).collect();
    for id in gridcraft_engine::coauthor::LOCAL_COMMANDS.iter().chain(gridcraft_engine::coauthor::LOCAL_QUERIES) {
        assert!(registered.contains(id), "the personal list names `{id}`, which is not a command");
    }
    for id in registered.iter().filter(|id| is_shared_command(id)) {
        assert!(!is_local_command(id), "{id}");
    }
    assert_eq!(registered.iter().filter(|id| is_shared_command(id)).count(), 34, "every shared command is registered");
}

#[test]
fn unshared_document_command_is_refused_with_a_reason() {
    let mut server = DeterministicServer::new();
    let mut client = coauthor(&["Data"]);
    server.join(&mut client, 10);
    let before = entered(&client);

    let result = client.submit("home.mergeCells", json!({"range":"A1:B2"}), ORIGIN_YEAR, 20);

    assert!(result.is_err(), "an unshared document command is not sent");
    assert_eq!(entered(&client), before, "nothing changed");
    assert_eq!(client.take_notices(), vec![LiveNotice::Unshared { command: "home.mergeCells".into() }]);
    assert!(client.take_notices().is_empty(), "notices are taken once");
}

#[test]
fn ui_defaults_become_explicit_targets() {
    let mut session = session_with_sheets(&["Data", "View"]);
    select(&mut session, "View", "B2");
    session.execute("selection.set", json!({"range":"B2:C3,E5","active":"E5"})).expect("selection moves");

    let bold = explicit_params(&session, "home.bold", json!({})).expect("bold targets");
    assert_eq!(bold, json!({"sheet":1,"range":"B2:C3,E5","on":true}));
    let named = explicit_params(&session, "home.bold", json!({"sheet":"Data","range":"A1","on":false})).expect("given targets stay");
    assert_eq!(named, json!({"sheet":0,"range":"A1","on":false}));

    let typed = explicit_params(&session, "cell.set", json!({"input":"typed"})).expect("cell targets");
    assert_eq!(typed, json!({"sheet":1,"cell":"E5","input":"typed"}));
    let number = explicit_params(&session, "cell.set", json!({"value":12})).expect("a number is entered as typed");
    assert_eq!(number, json!({"sheet":1,"cell":"E5","input":"12"}));

    let delete = explicit_params(&session, "home.deleteRows", json!({})).expect("rows target");
    assert_eq!(delete, json!({"sheet":1,"rows":"2:3,5:5"}));
    let delete_cols = explicit_params(&session, "home.deleteColumns", json!({})).expect("columns target");
    assert_eq!(delete_cols, json!({"sheet":1,"cols":"B:C,E:E"}));
    let insert = explicit_params(&session, "home.insertRows", json!({})).expect("insert target");
    assert_eq!(insert, json!({"sheet":1,"rows":"5:5"}));

    let sheet = explicit_params(&session, "home.insertSheet", json!({})).expect("sheet insert target");
    assert_eq!(sheet, json!({"name":"Sheet1","before":2}));
    let rename = explicit_params(&session, "sheet.rename", json!({"name":"Renamed"})).expect("rename target");
    assert_eq!(rename, json!({"sheet":1,"name":"Renamed"}));
    let size = explicit_params(&session, "home.increaseFontSize", json!({})).expect("size target");
    assert_eq!(size, json!({"sheet":1,"range":"B2:C3,E5","size":12.0}));
    let underline = explicit_params(&session, "home.underline", json!({})).expect("underline target");
    assert_eq!(underline, json!({"sheet":1,"range":"B2:C3,E5","on":true,"style":"single"}));

    assert!(explicit_params(&session, "home.mergeCells", json!({})).is_err(), "only shared commands are made explicit");

    // An explicit command names the same targets wherever the sender's view moves next.
    let mut client = GridCoauthor::new(live_config(), GridLiveEditor::new(session).expect("editor builds"));
    let mut server = DeterministicServer::new();
    let place = server.join(&mut client, 10);
    let proposal = submit(&mut client, &server, &place, "home.bold", bold, 20);
    let payload: Json = serde_json::from_str(&proposal.payload).expect("payload is JSON");
    assert_eq!(payload["command"]["on"], json!(true));
    assert_eq!(payload["command"]["sheet"], json!([0, 1]), "the View sheet's identity");
}

#[test]
fn viewer_cannot_submit_but_can_move_its_own_view() {
    let mut viewer = coauthor(&["Data", "View"]);
    join_with_access(&mut viewer, 3, false, 10);
    assert!(!viewer.can_edit());
    let before = entered(&viewer);

    let refused = viewer.submit("cell.set", json!({"sheet":0,"cell":"A1","input":"no"}), ORIGIN_YEAR, 20);
    assert!(matches!(refused, Err(GridLiveError::Local(LocalError::ReadOnly))), "{refused:?}");
    assert!(matches!(viewer.undo(21), Err(GridLiveError::Local(LocalError::ReadOnly))), "personal Undo is refused too");
    assert_eq!(entered(&viewer), before, "nothing was shown");
    assert!(viewer.tick(22).iter().all(|frame| !frame.contains("\"entry\"")), "nothing was sent");

    viewer.editor_mut().session_mut().execute("sheet.activate", json!({"sheet":"View"})).expect("the viewer changes sheet");
    viewer.editor_mut().session_mut().execute("selection.set", json!({"range":"D4"})).expect("the viewer moves");
    assert_eq!(active_cell(&viewer), (1, CellRef::new(3, 3)));
    assert_eq!(entered(&viewer), before, "moving the view changes no shared content");
}

#[test]
fn starting_from_file_bytes_gives_every_client_the_same_identities() {
    let mut source = session_with_sheets(&["Data", "Notes"]);
    source.execute("cell.set", json!({"sheet":"Data","cell":"B2","input":"=1+2"})).expect("content enters");
    source.execute("home.insertRows", json!({"sheet":"Data","rows":"1:1"})).expect("rows insert");
    let bytes = io::save_bytes(&source.doc().expect("document exists").wb, "Base.xlsx").expect("workbook saves");

    let first = GridCoauthor::start(Session::new(), LiveBase::File { bytes: &bytes }, live_config()).expect("first client starts");
    let second = GridCoauthor::start(session_with_sheets(&["Other"]), LiveBase::File { bytes: &bytes }, live_config()).expect("second client starts");

    assert_eq!(first.editor().ids(), second.editor().ids(), "fresh identities agree");
    assert_eq!(first.editor().ids().session_data().expect("data"), second.editor().ids().session_data().expect("data"));
    assert_eq!(first.editor().ids().sheet_id(1).expect("second sheet"), LiveId(0, 1), "base identities are participant 0");
    assert_eq!(entered(&first), entered(&second), "the same entered content");
    assert_eq!(second.editor().session().documents().len(), 1, "the base replaces what the session held");
    assert!(first.editor().session().is_coauthoring());

    let garbage = GridCoauthor::start(Session::new(), LiveBase::File { bytes: b"not a workbook" }, live_config());
    assert!(garbage.is_err(), "unreadable bytes are an error, not a crash");
}

#[test]
fn starting_from_a_checkpoint_keeps_its_identities() {
    let mut server = DeterministicServer::new();
    let mut alice = coauthor(&["Data"]);
    let place = server.join(&mut alice, 10);
    let insert = submit(&mut alice, &server, &place, "home.insertRows", json!({"sheet":0,"rows":"2:2"}), 20);
    let ordered = server.order(&insert, 1_795_219_200_000, 7);
    server.deliver(&mut alice, &ordered, 30);
    let checkpoint = alice.checkpoint(server.head()).expect("checkpoint writes");

    let stamp = collab_live::client::Stamp { at_ms: 1_795_219_200_000, seed: 7 };
    let joined = GridCoauthor::start(
        Session::new(),
        LiveBase::Checkpoint { bytes: &checkpoint.bytes, session_data: &checkpoint.session_data, stamp },
        live_config(),
    )
    .expect("a checkpoint base starts");

    assert_eq!(joined.editor().ids(), alice.editor().ids(), "the checkpoint's identities are kept");
    assert_eq!(entered(&joined), checkpoint.entered);
}

#[test]
fn presence_round_trips_by_identity() {
    let mut alice = live_coauthor(&["Data", "View"]);
    let bob = coauthor(&["Data", "View"]);
    select(alice.editor_mut().session_mut(), "View", "C3");
    alice.editor_mut().session_mut().execute("selection.set", json!({"range":"C3:D4","active":"C3"})).expect("range selects");
    alice.set_editing(true);

    let note = alice.presence(0).expect("a first note is sent");
    assert!(note.len() <= MAX_PRESENCE_BYTES);
    assert_eq!(
        bob.shown_presence(&note),
        Some(ShownPresence { sheet: 1, active: (2, 2), range: Some(RangeRef::parse("C3:D4").expect("range")), editing: true })
    );
    assert_eq!(alice.presence(1_000), None, "an unchanged view sends nothing");

    alice.editor_mut().session_mut().execute("selection.set", json!({"range":"E5"})).expect("moves");
    alice.set_editing(false);
    let moved = alice.presence(1_010).expect("a change is sent");
    assert_eq!(bob.shown_presence(&moved), Some(ShownPresence { sheet: 1, active: (4, 4), range: None, editing: false }));

    alice.editor_mut().session_mut().execute("selection.set", json!({"range":"F6"})).expect("moves again");
    assert_eq!(alice.presence(1_050), None, "at most ten notes a second");
    let later = alice.presence(1_110).expect("the held change goes once the interval passes");
    assert_eq!(bob.shown_presence(&later).map(|shown| shown.active), Some((5, 5)));
    assert_eq!(fingerprint(alice.editor().session(), DocId(0)).entered, entered(&bob), "presence never changes the workbook");
}

#[test]
fn presence_follows_a_remote_row_insert() {
    let mut server = DeterministicServer::new();
    let mut alice = coauthor(&["Data"]);
    let mut bob = coauthor(&["Data"]);
    let _alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);
    select(alice.editor_mut().session_mut(), "Data", "B5");
    let note = alice.presence(30).expect("note");

    let insert = submit(&mut bob, &server, &bob_place, "home.insertRows", json!({"sheet":0,"rows":"2:3"}), 40);
    let ordered = server.order(&insert, 1_795_219_200_000, 3);
    server.deliver_both(&mut alice, &mut bob, &ordered, 50);

    assert_eq!(bob.shown_presence(&note).map(|shown| shown.active), Some((6, 1)), "the note names a row, not a position");
}

#[test]
fn presence_on_a_deleted_row_is_dropped() {
    let mut server = DeterministicServer::new();
    let mut alice = coauthor(&["Data"]);
    let mut bob = coauthor(&["Data"]);
    let _alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);
    select(alice.editor_mut().session_mut(), "Data", "B5");
    let note = alice.presence(30).expect("note");
    alice.editor_mut().session_mut().execute("selection.set", json!({"range":"A1:C2"})).expect("range");
    let range_note = alice.presence(200).expect("range note");

    let delete = submit(&mut bob, &server, &bob_place, "home.deleteRows", json!({"sheet":0,"rows":"5:5"}), 40);
    let ordered = server.order(&delete, 1_795_219_200_000, 3);
    server.deliver_both(&mut alice, &mut bob, &ordered, 50);

    assert_eq!(bob.shown_presence(&note), None, "a note on a deleted row is not drawn");
    assert!(bob.shown_presence(&range_note).is_some(), "a note on surviving rows still is");
}

#[test]
fn malformed_presence_is_ignored() {
    let alice = coauthor(&["Data"]);
    let good = {
        let mut sender = live_coauthor(&["Data"]);
        sender.presence(0).expect("note")
    };
    assert!(alice.shown_presence(&good).is_some());

    let mut value: Json = serde_json::from_str(&good).expect("note is JSON");
    value["future"] = json!(true);
    let unknown_field = value.to_string();
    let mut other_sheet: Json = serde_json::from_str(&good).expect("note is JSON");
    other_sheet["active"]["sheet"] = json!([9, 9]);
    let oversized = format!("{}{}", good, " ".repeat(MAX_PRESENCE_BYTES));

    for bad in ["", "null", "{}", "[1,2]", "{\"sheet\":", unknown_field.as_str(), other_sheet.to_string().as_str(), oversized.as_str()] {
        assert_eq!(alice.shown_presence(bad), None, "{bad:.80}");
    }
}

#[test]
fn no_effect_notice_keeps_the_typed_text() {
    let mut server = DeterministicServer::new();
    let mut alice = coauthor(&["Data"]);
    let mut bob = coauthor(&["Data"]);
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let typed = submit(&mut alice, &server, &alice_place, "cell.set", json!({"sheet":0,"cell":"B5","input":"my careful words"}), 30);
    let delete = submit(&mut bob, &server, &bob_place, "home.deleteRows", json!({"sheet":0,"rows":"5:5"}), 31);
    let deleted = server.order(&delete, 1_795_219_200_000, 1);
    let late = server.order(&typed, 1_795_219_201_000, 2);
    let steps: Vec<Step> = [&deleted, &late].into_iter().map(|entry| server.deliver(&mut alice, entry, 40)).collect();
    server.deliver(&mut bob, &deleted, 41);
    server.deliver(&mut bob, &late, 42);

    assert!(steps.iter().flat_map(|step| &step.events).any(|event| matches!(event, Event::Applied { own: true, .. })), "Alice's change was ordered");
    let notices = alice.take_notices();
    assert_eq!(
        notices,
        vec![LiveNotice::NotApplied { seq: late.seq, kept_text: Some("my careful words".into()), reason: NotAppliedReason::RowDeleted }]
    );
    assert!(bob.take_notices().is_empty(), "the other person is not told about Alice's change");
    assert_eq!(entered(&alice), entered(&bob), "both agree");
}

#[test]
fn notice_debug_hides_the_kept_text() {
    let notice = LiveNotice::NotApplied { seq: Seq(4), kept_text: Some("salary 120000".into()), reason: NotAppliedReason::RowDeleted };

    let debug = format!("{notice:?}");
    assert!(!debug.contains("salary") && !debug.contains("120000"), "{debug}");
    assert!(debug.contains("13"), "Debug shows the length: {debug}");

    let json = serde_json::to_value(&notice).expect("notice serializes for the page");
    assert_eq!(json["kind"], json!("not_applied"));
    assert_eq!(json["kept_text"], json!("salary 120000"), "the page still gets the text to show its owner");
    assert_eq!(json["reason"], json!("row_deleted"));
}

#[test]
fn room_events_become_notices() {
    let mut client = coauthor(&["Data"]);
    join_with_access(&mut client, 2, true, 10);
    let _ = client.take_notices();

    client.receive(&json!({"type":"access","can_edit":false}).to_string(), 20).expect("access change is accepted");
    assert_eq!(client.take_notices(), vec![LiveNotice::ReadOnly { read_only: true }]);
    assert!(!client.can_edit());
}

/// Every command a live document runs as personal leaves the shared workbook exactly as it was,
/// whatever parameters it is given, so a personal command (a viewer's too) can never change the
/// document behind the log.
#[test]
fn local_commands_leave_the_shared_workbook_unchanged() {
    let variants = [json!({}), json!({"add":"A1"}), json!({"remove":"A1"}), json!({"range":"A1:B2"}), json!({"cell":"A1"}), json!({"sheet":"View"})];
    let mut checked = 0;
    for spec in command_specs() {
        if route(spec.id) != Route::Local {
            continue;
        }
        for params in &variants {
            let mut session = session_with_sheets(&["Data", "View"]);
            session.execute("cell.set", json!({"sheet":"Data","cell":"A1","input":"7"})).expect("seed value");
            session.execute("cell.set", json!({"sheet":"Data","cell":"B2","input":"=A1*2"})).expect("seed formula");
            let before = fingerprint(&session, DocId(0)).entered;
            let _ = session.execute(spec.id, params.clone());
            assert_eq!(fingerprint(&session, DocId(0)).entered, before, "`{}` with {params} changed the shared workbook", spec.id);
        }
        checked += 1;
    }
    assert!(checked >= gridcraft_engine::coauthor::LOCAL_COMMANDS.len(), "every personal command was run");
    assert_eq!(route("formulas.watchWindow"), Route::Refused(UNSHARED_REASON), "the watch list is kept in the workbook");
}

#[test]
fn filter_commands_target_the_sheet_filter_not_the_selection() {
    let mut session = session_with_sheets(&["Data", "View"]);
    session.execute("data.filterSet", json!({"sheet":"View","range":"A1:C5"})).expect("filter installs");
    select(&mut session, "View", "E9");
    for id in ["data.clearFilter", "data.reapply", "data.filterRemove"] {
        let params = explicit_params(&session, id, json!({})).expect("filter target");
        assert_eq!(params, json!({"sheet":1,"range":"A1:C5"}), "{id}");
    }
    let by = explicit_params(&session, "data.filterBy", json!({"column":"B","values":["x"]})).expect("filter target");
    assert_eq!(by, json!({"sheet":1,"range":"A1:C5","column":"B","values":["x"]}));

    // Through the entry point, the explicit target is the filter itself, so the change applies.
    let reapply = explicit_params(&session, "data.reapply", json!({})).expect("filter target");
    let mut client = GridCoauthor::new(live_config(), GridLiveEditor::new(session).expect("editor builds"));
    let mut server = DeterministicServer::new();
    let place = server.join(&mut client, 10);
    let proposal = submit(&mut client, &server, &place, "data.reapply", reapply, 20);
    let ordered = server.order(&proposal, 1_795_219_200_000, 3);
    let step = server.deliver(&mut client, &ordered, 30);
    assert!(
        step.events.iter().any(|event| matches!(event, Event::Applied { own: true, effect: collab_live::client::Effect::Applied, .. })),
        "reapplying the filter takes effect: {step:?}"
    );

    let mut bare = session_with_sheets(&["Data"]);
    select(&mut bare, "Data", "B2");
    assert!(explicit_params(&bare, "data.clearFilter", json!({})).is_err(), "no filter, no target");
}

#[test]
fn presence_is_sent_again_after_a_reconnect() {
    let mut alice = live_coauthor(&["Data"]);
    select(alice.editor_mut().session_mut(), "Data", "C3");
    assert!(alice.presence(0).is_some(), "a first note is sent");
    assert_eq!(alice.presence(500), None, "an unchanged note is not repeated");
    alice.disconnected(600);
    assert_eq!(alice.presence(650), None, "nothing while offline");
    rejoin(&mut alice, 700);
    assert!(alice.presence(800).is_some(), "the room dropped the note with the connection, so it goes again");
}

#[test]
fn clicking_the_active_alignment_again_returns_to_general() {
    let mut session = session_with_sheets(&["Data"]);
    select(&mut session, "Data", "A1");
    let first = explicit_params(&session, "home.alignCenter", json!({})).expect("alignment target");
    assert_eq!(first["on"], json!(true));
    session.execute("home.alignCenter", first).expect("centres");
    let again = explicit_params(&session, "home.alignCenter", json!({})).expect("alignment target");
    assert_eq!(again["on"], json!(false), "the sender computes the toggle");
    session.execute("home.alignCenter", again).expect("returns to General");
    let third = explicit_params(&session, "home.alignCenter", json!({})).expect("alignment target");
    assert_eq!(third["on"], json!(true), "the cell is back to General, so the next click centres it");
}

/// Every notice reaches the page as JSON with its kind; none fails to serialise.
#[test]
fn every_notice_serialises() {
    let notices = [
        LiveNotice::NotApplied { seq: Seq(3), kept_text: Some("typed".into()), reason: NotAppliedReason::RowDeleted },
        LiveNotice::ChangeRefused { reason: protocol::live::RefusedReason::ReadOnly },
        LiveNotice::UndoLeft { cells: 2 },
        LiveNotice::UndoRefused { reason: "no".into() },
        LiveNotice::Unshared { command: "home.mergeCells".into() },
        LiveNotice::Paused { entries: 1, bytes: 2 },
        LiveNotice::Resumed,
        LiveNotice::Returned { not_applied: 1 },
        LiveNotice::ArchivedReturn,
        LiveNotice::OthersReturning { participant: protocol::live::ParticipantNo(4) },
        LiveNotice::ReadOnly { read_only: true },
        LiveNotice::CannotOpen,
        LiveNotice::Incompatible,
        LiveNotice::BuildRefused,
        LiveNotice::Removed,
    ];
    for notice in &notices {
        let json = serde_json::to_value(notice).unwrap_or_else(|error| panic!("{notice:?} does not serialise: {error}"));
        assert!(json["kind"].is_string(), "{notice:?} names its kind: {json}");
    }
    assert_eq!(serde_json::to_value(LiveNotice::ReadOnly { read_only: false }).expect("serialises"), json!({"kind":"read_only","read_only":false}));
}

fn fingerprints(step: &Step) -> Vec<Json> {
    step.send.iter().filter_map(|frame| serde_json::from_str::<Json>(frame).ok()).filter(|frame| frame["type"] == "fingerprint").collect()
}

/// The room compares entered digests at every non-zero multiple of 64 positions and resyncs a
/// client that disagrees (G4 `live_room.rs` `fingerprint`, `AGREEMENT_EVERY`). Every editing
/// client reports at those positions, and a client whose workbook drifted reports a different
/// digest, which the room's decision catches.
#[test]
fn entered_agreement_is_reported_at_every_64th_position_and_catches_drift() {
    let mut server = DeterministicServer::new();
    let mut alice = coauthor(&["Data"]);
    let mut bob = coauthor(&["Data"]);
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 10);
    let mut reports: Vec<(u64, Json, Json)> = Vec::new();
    for n in 1..=128_u64 {
        if n == 100 {
            // Bob's copy changes outside the log: his digest must stop agreeing.
            bob.editor_mut().session_mut().execute("cell.set", json!({"sheet":"Data","cell":"Z9","input":"drift"})).expect("drift");
        }
        let now = 100 + n * 100;
        let step = alice.submit("cell.set", json!({"sheet":0,"cell":"A1","input":n.to_string()}), ORIGIN_YEAR, now).expect("submits");
        assert!(fingerprints(&step).is_empty(), "nothing is reported for a change still waiting");
        let proposal = server.one_proposal(&alice_place, &step);
        let ordered = server.order(&proposal, 1_795_219_200_000, 7);
        let (alice_step, bob_step) = server.deliver_both(&mut alice, &mut bob, &ordered, now + 1);
        let (from_alice, from_bob) = (fingerprints(&alice_step), fingerprints(&bob_step));
        if n % 64 == 0 {
            assert_eq!((from_alice.len(), from_bob.len()), (1, 1), "both editors report at {n}");
            reports.push((n, from_alice[0].clone(), from_bob[0].clone()));
        } else {
            assert!(from_alice.is_empty() && from_bob.is_empty(), "no report at {n}");
        }
    }
    for (at, alice_report, bob_report) in &reports {
        for report in [alice_report, bob_report] {
            assert_eq!(report["at"], json!(at), "the report names its exact position");
            let entered = report["entered"].as_str().expect("entered is text");
            assert!(entered.len() == 64 && entered.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()), "the room's digest form");
        }
    }
    assert_eq!(reports[0].1["entered"], reports[0].2["entered"], "at 64 both copies agree");
    assert_ne!(reports[1].1["entered"], reports[1].2["entered"], "at 128 the drifted copy disagrees, so the room resyncs it");
}

#[test]
fn a_viewer_does_not_report_agreement() {
    let mut server = DeterministicServer::new();
    let mut alice = coauthor(&["Data"]);
    let mut viewer = coauthor(&["Data"]);
    let place = server.join(&mut alice, 10);
    join_with_access(&mut viewer, 9, false, 10);
    for n in 1..=64_u64 {
        let proposal = submit(&mut alice, &server, &place, "cell.set", json!({"sheet":0,"cell":"A1","input":n.to_string()}), 100 + n * 100);
        let ordered = server.order(&proposal, 1_795_219_200_000, 7);
        let step = server.deliver(&mut viewer, &ordered, 150 + n * 100);
        assert!(fingerprints(&step).is_empty(), "the room takes reports from editors only");
    }
}

#[test]
fn presence_waits_for_the_live_phase() {
    let mut alice = coauthor(&["Data"]);
    assert_eq!(alice.presence(10), None, "no note before the client is live");
    let mut server = DeterministicServer::new();
    server.join(&mut alice, 30);
    assert!(alice.presence(40).is_some(), "the first note goes once live");
}

#[test]
fn starting_empty_ignores_the_hosts_workbook() {
    let mut session = session_with_sheets(&["Other", "Sheets"]);
    session.execute("cell.set", json!({"sheet":"Other","cell":"A1","input":"mine"})).expect("seed");
    let started = GridCoauthor::start(session, LiveBase::Empty, live_config()).expect("an empty base starts");
    let blank = GridLiveEditor::blank().expect("blank editor");
    assert_eq!(started.editor().ids(), blank.ids(), "the empty base is the same on every machine");
    assert_eq!(entered(&started), fingerprint(blank.session(), DocId(0)).entered);
}
