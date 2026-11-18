use collab_core::ids::Seq;
use collab_live::client::LiveConfig;
use gridcraft_core::{CellRef, Value};
use gridcraft_engine::coauthor::{GRIDCRAFT_LIVE_FORMAT, GridCoauthor, GridLiveEditor, GuardedPatch, PersonalHistory, UndoAction};
use gridcraft_model::Style;
use protocol::live::MAX_PAYLOAD_BYTES;
use serde_json::json;
use std::sync::Arc;

use crate::live::{ClientPlace, DeterministicServer, OrderedEntry, ProposedEntry, session_with_sheets};

fn live_config() -> LiveConfig {
    LiveConfig { format: GRIDCRAFT_LIVE_FORMAT.to_string(), crashed_at: None }
}

fn initial_session() -> gridcraft_engine::Session {
    let mut session = session_with_sheets(&["Sheet1"]);
    session.execute("cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"original"})).expect("initial value is set");
    session
}

fn client() -> GridCoauthor {
    GridCoauthor::new(live_config(), GridLiveEditor::new(initial_session()).expect("editor builds"))
}

fn value(client: &GridCoauthor) -> Value {
    client.editor().session().doc().expect("document exists").wb.sheet(0).expect("sheet exists").value(CellRef::new(0, 0))
}

fn style(client: &GridCoauthor) -> Style {
    let document = client.editor().session().doc().expect("document exists");
    let sheet = document.wb.sheet(0).expect("sheet exists");
    document.wb.styles.get(sheet.style_id(CellRef::new(0, 0))).clone()
}

fn propose(
    server: &DeterministicServer,
    client: &mut GridCoauthor,
    place: &ClientPlace,
    id: &str,
    params: serde_json::Value,
    now_ms: u64,
) -> ProposedEntry {
    let step = client.submit(id, params, 2026, now_ms).expect("shared command submits");
    server.one_proposal(place, &step)
}

fn order_and_deliver(
    server: &mut DeterministicServer,
    proposal: &ProposedEntry,
    sender: &mut GridCoauthor,
    peer: &mut GridCoauthor,
    now_ms: u64,
) -> OrderedEntry {
    let ordered = server.order(proposal, 1_795_219_200_000 + now_ms as i64, 41);
    server.deliver(sender, &ordered, now_ms);
    server.deliver(peer, &ordered, now_ms.saturating_add(1));
    ordered
}

#[test]
fn content_undo_restores_only_unchanged_fields() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let edit = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"alice"}), 30);
    order_and_deliver(&mut server, &edit, &mut alice, &mut bob, 40);
    let format = propose(&server, &mut bob, &bob_place, "home.bold", json!({"sheet":"Sheet1","range":"A1","on":true}), 50);
    order_and_deliver(&mut server, &format, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("content undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert_eq!(value(&alice), Value::text("original"));
    assert_eq!(value(&bob), Value::text("original"));
    assert!(style(&alice).font.bold, "the later format remains");
    assert!(style(&bob).font.bold, "the peer keeps the same later format");
    let report = alice.last_undo_report().expect("undo report is retained");
    assert_eq!((report.restored, report.left), (1, 0));
}

#[test]
fn format_undo_restores_only_matching_style_leaves() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let size = propose(&server, &mut alice, &alice_place, "home.fontSize", json!({"sheet":"Sheet1","range":"A1","size":18}), 30);
    order_and_deliver(&mut server, &size, &mut alice, &mut bob, 40);
    let fill = propose(&server, &mut bob, &bob_place, "home.fillColor", json!({"sheet":"Sheet1","range":"A1","color":"#123456"}), 50);
    order_and_deliver(&mut server, &fill, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("format undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    let alice_style = style(&alice);
    let bob_style = style(&bob);
    assert_eq!(alice_style.font.size, Style::default().font.size);
    assert_eq!(alice_style.fill, bob_style.fill);
    assert_ne!(alice_style.fill, Style::default().fill, "the later fill remains");
}

#[test]
fn undo_is_personal_and_is_a_new_entry() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let alice_edit = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"alice"}), 30);
    let first = order_and_deliver(&mut server, &alice_edit, &mut alice, &mut bob, 40);
    let bob_edit = propose(&server, &mut bob, &bob_place, "home.bold", json!({"sheet":"Sheet1","range":"A1","on":true}), 50);
    let second = order_and_deliver(&mut server, &bob_edit, &mut bob, &mut alice, 60);

    assert_eq!((alice.history().undo_len(), bob.history().undo_len()), (1, 1));
    assert!(alice.history().has_undo_action(1));
    assert!(bob.history().has_undo_action(1));

    let undo = alice.undo(70).expect("undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    let third = order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert_eq!(first.seq, Seq(1));
    assert_eq!(second.seq, Seq(2));
    assert_eq!(third.seq, Seq(3), "undo advances the shared log");
    assert_eq!((alice.history().undo_len(), alice.history().redo_len()), (0, 1));
    assert_eq!((bob.history().undo_len(), bob.history().redo_len()), (1, 0), "Alice's undo never changes Bob's history");

    let redo = alice.redo(90).expect("redo submits as a guarded entry");
    let proposal = server.one_proposal(&alice_place, &redo);
    let fourth = order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 100);
    assert_eq!(fourth.seq, Seq(4), "redo also advances the shared log");
    assert_eq!(value(&alice), Value::text("alice"));
    assert!(style(&alice).font.bold, "redo changes content without removing Bob's format");
    assert_eq!((alice.history().undo_len(), alice.history().redo_len()), (1, 0));
    assert_eq!((bob.history().undo_len(), bob.history().redo_len()), (1, 0), "Alice's redo also stays out of Bob's history");
}

#[test]
fn refused_undo_stays_on_stack() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 20);

    let edit = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"alice"}), 30);
    order_and_deliver(&mut server, &edit, &mut alice, &mut bob, 40);
    let undo = alice.undo(50).expect("first undo submits");
    let refused = server.one_proposal(&alice_place, &undo);
    assert_eq!(value(&alice), Value::text("original"), "the optimistic undo is visible");

    alice.receive(&json!({"type":"refused","local":refused.local.0,"reason":"too_large"}).to_string(), 60).expect("refusal is accepted");

    assert_eq!(value(&alice), Value::text("alice"), "the refused optimistic undo rolls back");
    assert_eq!((alice.history().undo_len(), alice.history().redo_len()), (1, 0));
    let retry = alice.undo(70).expect("the same action remains available");
    assert_eq!(server.proposals(&alice_place, &retry).len(), 1);

    let mut large_session = session_with_sheets(&["Sheet1"]);
    let large_text = "x".repeat(MAX_PAYLOAD_BYTES / 2);
    {
        let document = large_session.doc_mut().expect("large document exists");
        let workbook = Arc::make_mut(&mut document.wb);
        let sheet = Arc::make_mut(workbook.sheets.get_mut(0).expect("large sheet exists"));
        sheet.cells.set(CellRef::new(0, 0), gridcraft_model::Cell::value(Value::text(large_text.as_str())));
        sheet.cells.set(CellRef::new(0, 1), gridcraft_model::Cell::value(Value::text(large_text.as_str())));
        sheet.cells.set(CellRef::new(0, 2), gridcraft_model::Cell::value(Value::text(large_text.as_str())));
    }
    let mut large = GridCoauthor::new(live_config(), GridLiveEditor::new(large_session).expect("large editor builds"));
    let mut large_server = DeterministicServer::new();
    let large_place = large_server.join(&mut large, 100);
    let clear = propose(&large_server, &mut large, &large_place, "edit.clearContents", json!({"sheet":"Sheet1","range":"A1:C1"}), 110);
    let ordered = large_server.order(&clear, 1_795_219_200_000, 42);
    large_server.deliver(&mut large, &ordered, 120);
    let first = large.undo(130).expect_err("an oversized guarded reverse is refused").to_string();
    let second = large.undo(140).expect_err("the oversized action stays available").to_string();
    assert!(first.contains("This change is too large to undo while sharing"), "{first}");
    assert!(second.contains("This change is too large to undo while sharing"), "{second}");
    assert_eq!((large.history().undo_len(), large.history().redo_len()), (1, 0));
}

fn action(action: u64, retained_bytes: usize) -> UndoAction {
    UndoAction {
        action,
        label: format!("Action {action}"),
        undo: GuardedPatch::Cells(Vec::new()),
        redo: GuardedPatch::Cells(Vec::new()),
        retained_bytes,
    }
}

#[test]
fn personal_history_obeys_count_and_byte_budgets() {
    let mut count_limited = PersonalHistory::default();
    let mut other_person = PersonalHistory::default();
    other_person.record(action(900, 1));
    for number in 1..=101 {
        count_limited.record(action(number, 1));
    }
    assert_eq!(count_limited.undo_len(), 100);
    assert!(!count_limited.has_undo_action(1));
    assert!(count_limited.has_undo_action(101));
    assert!(other_person.has_undo_action(900), "eviction is local to one person's history");

    let mut byte_limited = PersonalHistory::default();
    for number in 1..=3 {
        byte_limited.record(action(number, 64 * 1024 * 1024));
    }
    assert_eq!(byte_limited.undo_len(), 2);
    assert!(!byte_limited.has_undo_action(1));
    assert!(byte_limited.has_undo_action(3));
}

#[test]
fn pending_action_that_becomes_no_effect_is_not_recorded() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let pending = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"alice"}), 30);
    let deletion = propose(&server, &mut bob, &bob_place, "home.deleteRows", json!({"sheet":"Sheet1","rows":"1:1"}), 31);
    let ordered_delete = server.order(&deletion, 1_795_219_200_000, 41);
    server.deliver(&mut alice, &ordered_delete, 40);
    server.deliver(&mut bob, &ordered_delete, 41);
    let ordered_pending = server.order(&pending, 1_795_219_200_001, 42);
    server.deliver(&mut alice, &ordered_pending, 50);
    server.deliver(&mut bob, &ordered_pending, 51);

    assert_eq!(alice.history().undo_len(), 0, "a replay that no longer has a target leaves no personal Undo action");
}
