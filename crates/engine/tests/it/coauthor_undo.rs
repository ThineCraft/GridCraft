use collab_core::ids::Seq;
use collab_live::client::{Effect, Event, LiveConfig};
use gridcraft_core::{CellRef, MAX_ROWS, Value};
use gridcraft_engine::coauthor::{
    GRIDCRAFT_LIVE_FORMAT, GridCoauthor, GridLiveEditor, GuardedPatch, GuardedStructural, PERSONAL_HISTORY_BYTES, PersonalHistory, SharedCommand,
    UndoAction, decode_entry,
};
use gridcraft_model::{SheetProtection, Style};
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

fn formula_client() -> GridCoauthor {
    let mut session = initial_session();
    session.execute("cell.set", json!({"sheet":"Sheet1","cell":"B1","input":"=A1"})).expect("formula is set");
    GridCoauthor::new(live_config(), GridLiveEditor::new(session).expect("editor builds"))
}

fn numeric_client() -> GridCoauthor {
    let mut session = session_with_sheets(&["Sheet1"]);
    session.execute("cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"100"})).expect("number is set");
    GridCoauthor::new(live_config(), GridLiveEditor::new(session).expect("editor builds"))
}

fn value(client: &GridCoauthor) -> Value {
    client.editor().session().doc().expect("document exists").wb.sheet(0).expect("sheet exists").value(CellRef::new(0, 0))
}

fn style(client: &GridCoauthor) -> Style {
    let document = client.editor().session().doc().expect("document exists");
    let sheet = document.wb.sheet(0).expect("sheet exists");
    document.wb.styles.get(sheet.style_id(CellRef::new(0, 0))).clone()
}

fn revision(client: &GridCoauthor) -> u64 {
    client.editor().session().doc().expect("document exists").revision
}

fn protect_sheet(client: &mut GridCoauthor, protection: SheetProtection) {
    let document = client.editor_mut().session_mut().doc_mut().expect("document exists");
    Arc::make_mut(&mut document.wb).sheets.get_mut(0).map(Arc::make_mut).expect("sheet exists").protection = Some(protection);
}

fn protect_workbook_structure(client: &mut GridCoauthor) {
    let document = client.editor_mut().session_mut().doc_mut().expect("document exists");
    Arc::make_mut(&mut document.wb).protected_structure = true;
}

fn formula(client: &GridCoauthor, cell: &str) -> Option<String> {
    let at = CellRef::parse(cell).expect("test cell parses");
    client
        .editor()
        .session()
        .doc()
        .expect("document exists")
        .wb
        .sheet(0)
        .expect("sheet exists")
        .cell(at)
        .and_then(|cell| cell.formula.as_ref())
        .map(|formula| formula.text.clone())
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
fn content_undo_leaves_a_later_value_on_the_identical_cell() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let alice_edit = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"alice"}), 30);
    order_and_deliver(&mut server, &alice_edit, &mut alice, &mut bob, 40);
    let bob_edit = propose(&server, &mut bob, &bob_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"bob"}), 50);
    order_and_deliver(&mut server, &bob_edit, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("content undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert_eq!(value(&alice), Value::text("bob"));
    assert_eq!(value(&bob), Value::text("bob"));
    let report = alice.last_undo_report().expect("undo report is retained");
    assert_eq!((report.restored, report.left), (0, 1));
}

#[test]
fn format_undo_leaves_a_later_change_to_the_identical_style_leaf() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let alice_size = propose(&server, &mut alice, &alice_place, "home.fontSize", json!({"sheet":"Sheet1","range":"A1","size":18}), 30);
    order_and_deliver(&mut server, &alice_size, &mut alice, &mut bob, 40);
    let bob_size = propose(&server, &mut bob, &bob_place, "home.fontSize", json!({"sheet":"Sheet1","range":"A1","size":20}), 50);
    order_and_deliver(&mut server, &bob_size, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("format undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert_eq!(style(&alice).font.size, 20.0);
    assert_eq!(style(&bob).font.size, 20.0);
    let report = alice.last_undo_report().expect("undo report is retained");
    assert_eq!((report.restored, report.left), (0, 1));
}

#[test]
fn formula_undo_resolves_references_after_a_row_is_inserted() {
    let mut server = DeterministicServer::new();
    let mut alice = formula_client();
    let mut bob = formula_client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let replace = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"B1","input":"replaced"}), 30);
    order_and_deliver(&mut server, &replace, &mut alice, &mut bob, 40);
    let insert = propose(&server, &mut bob, &bob_place, "home.insertRows", json!({"sheet":"Sheet1","rows":"1:1"}), 50);
    order_and_deliver(&mut server, &insert, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("formula undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert_eq!(formula(&alice, "B2").as_deref(), Some("A2"));
    assert_eq!(formula(&bob, "B2").as_deref(), Some("A2"));
}

#[test]
fn formula_undo_turns_a_deleted_identity_into_ref() {
    let mut server = DeterministicServer::new();
    let mut alice = formula_client();
    let mut bob = formula_client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let replace = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"B1","input":"replaced"}), 30);
    order_and_deliver(&mut server, &replace, &mut alice, &mut bob, 40);
    let delete = propose(&server, &mut bob, &bob_place, "home.deleteColumns", json!({"sheet":"Sheet1","cols":"A:A"}), 50);
    order_and_deliver(&mut server, &delete, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("formula undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert_eq!(formula(&alice, "A1").as_deref(), Some("#REF!"));
    assert_eq!(formula(&bob, "A1").as_deref(), Some("#REF!"));
}

#[test]
fn content_undo_is_independent_of_later_number_formats() {
    let mut alice = numeric_client();
    let mut bob = numeric_client();
    let mut server = DeterministicServer::new();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let replace = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"replaced"}), 30);
    order_and_deliver(&mut server, &replace, &mut alice, &mut bob, 40);
    let percent = propose(&server, &mut bob, &bob_place, "home.numberFormat", json!({"sheet":"Sheet1","range":"A1","format":"Percentage"}), 50);
    order_and_deliver(&mut server, &percent, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("content undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert_eq!(value(&alice), Value::Number(100.0));
    assert_eq!(value(&bob), Value::Number(100.0));
    assert_eq!(style(&alice).num_fmt.as_str(), "0.00%");
}

#[test]
fn formula_undo_is_independent_of_a_later_text_format() {
    let mut server = DeterministicServer::new();
    let mut alice = formula_client();
    let mut bob = formula_client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let text = propose(&server, &mut bob, &bob_place, "home.numberFormat", json!({"sheet":"Sheet1","range":"B1","code":"@"}), 30);
    order_and_deliver(&mut server, &text, &mut bob, &mut alice, 40);
    let replace = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"B1","input":"replaced"}), 50);
    order_and_deliver(&mut server, &replace, &mut alice, &mut bob, 60);

    let undo = alice.undo(70).expect("formula undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert_eq!(formula(&alice, "B1").as_deref(), Some("A1"));
    assert_eq!(formula(&bob, "B1").as_deref(), Some("A1"));
}

#[test]
fn content_undo_round_trips_the_exact_stored_number() {
    let mut session = session_with_sheets(&["Sheet1"]);
    session.execute("cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"1.234567890123456"})).expect("precise number is set");
    let mut alice = GridCoauthor::new(live_config(), GridLiveEditor::new(session).expect("editor builds"));
    let mut peer = numeric_client();
    let mut server = DeterministicServer::new();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut peer, 20);
    let replace = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"replaced"}), 30);
    let ordered = server.order(&replace, 1_795_219_200_030, 41);
    server.deliver(&mut alice, &ordered, 40);

    let undo = alice.undo(50).expect("precise-number undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    let ordered = server.order(&proposal, 1_795_219_200_050, 42);
    server.deliver(&mut alice, &ordered, 60);

    assert_eq!(value(&alice), Value::Number(1.234567890123456));
}

#[test]
fn color_changes_from_default_have_personal_undo_and_redo() {
    let cases = [
        ("home.fontColor", json!({"sheet":"Sheet1","range":"A1","color":"#123456"}), "font"),
        ("home.fillColor", json!({"sheet":"Sheet1","range":"A1","color":"#123456"}), "fill"),
        ("home.borders", json!({"sheet":"Sheet1","range":"A1","preset":"all","style":"thin","color":"#123456"}), "border"),
    ];
    for (command, params, path) in cases {
        let mut server = DeterministicServer::new();
        let mut alice = client();
        let mut bob = client();
        let alice_place = server.join(&mut alice, 10);
        server.join(&mut bob, 20);

        let color = propose(&server, &mut alice, &alice_place, command, params, 30);
        order_and_deliver(&mut server, &color, &mut alice, &mut bob, 40);
        assert_eq!(alice.history().undo_len(), 1, "{path} color is recorded");

        let undo = alice.undo(50).expect("color undo submits");
        let undo_proposal = server.one_proposal(&alice_place, &undo);
        order_and_deliver(&mut server, &undo_proposal, &mut alice, &mut bob, 60);
        assert_eq!(style(&alice), Style::default(), "{path} color returns to Auto");

        let redo = alice.redo(70).expect("color redo submits");
        let redo_proposal = server.one_proposal(&alice_place, &redo);
        order_and_deliver(&mut server, &redo_proposal, &mut alice, &mut bob, 80);
        assert_ne!(style(&alice), Style::default(), "{path} color is restored by redo");
    }
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

    let before_undo_revision = revision(&alice);
    let undo = alice.undo(70).expect("undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    let third = order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert_eq!(first.seq, Seq(1));
    assert_eq!(second.seq, Seq(2));
    assert_eq!(third.seq, Seq(3), "undo advances the shared log");
    assert!(revision(&alice) > before_undo_revision, "a guarded undo advances the document revision used by caches");
    assert_eq!((alice.history().undo_len(), alice.history().redo_len()), (0, 1));
    assert_eq!((bob.history().undo_len(), bob.history().redo_len()), (1, 0), "Alice's undo never changes Bob's history");

    let before_redo_revision = revision(&alice);
    let redo = alice.redo(90).expect("redo submits as a guarded entry");
    let proposal = server.one_proposal(&alice_place, &redo);
    let fourth = order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 100);
    assert_eq!(fourth.seq, Seq(4), "redo also advances the shared log");
    assert!(revision(&alice) > before_redo_revision, "a guarded redo advances the document revision used by caches");
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

    let mut single_oversized = PersonalHistory::default();
    single_oversized.record(action(1, PERSONAL_HISTORY_BYTES + 1));
    assert_eq!(single_oversized.undo_len(), 0, "one action larger than the whole budget is evicted immediately");
}

#[test]
fn guarded_payloads_are_closed_typed_and_bounded() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 20);

    let edit = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"=B1"}), 30);
    order_and_deliver(&mut server, &edit, &mut alice, &mut bob, 40);
    let undo = alice.undo(50).expect("guarded undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    assert!(!proposal.payload.contains("\"text\""), "formula internals never enter the wire: {}", proposal.payload);
    assert!(!proposal.payload.contains("python"), "Python internals never enter the wire: {}", proposal.payload);
    decode_entry(&proposal.payload).expect("the generated typed guarded patch decodes");

    let mut forged: serde_json::Value = serde_json::from_str(&proposal.payload).expect("proposal is JSON");
    forged["command"]["patch"]["Cells"][0]["restore"] = json!({"formula":{"text":"B1","array":null,"dynamic":false}});
    let raw_state = serde_json::to_string(&forged).expect("forged entry encodes");
    assert!(decode_entry(&raw_state).is_err(), "raw Formula state must be refused during decoding");

    let mut unknown: serde_json::Value = serde_json::from_str(&proposal.payload).expect("proposal is JSON");
    unknown["command"]["patch"]["Cells"][0]["restore"]["unknown"] = json!(true);
    let unknown = serde_json::to_string(&unknown).expect("unknown-field entry encodes");
    assert!(decode_entry(&unknown).is_err(), "unknown guarded cell fields must be refused");

    let unknown_style = json!({
        "version":1,
        "action":98,
        "command":{
            "id":"edit.guardedPatch",
            "patch":{"Styles":[{
                "target":{"kind":"cell","cell":{"sheet":[0,0],"row":[0,0],"col":[0,0]}},
                "path":["font","bold"],
                "expected":{"raw":true},
                "restore":{"font":{"bold":false}}
            }]}
        }
    })
    .to_string();
    assert!(decode_entry(&unknown_style).is_err(), "guarded styles accept only the typed StylePatch fields");

    let duplicate = proposal.payload.replacen("\"input\":", "\"input\":\"forged\",\"input\":", 1);
    assert!(decode_entry(&duplicate).is_err(), "duplicate keys must be refused before application");

    let mut nested = json!({"Cells":[]});
    for _ in 0..130 {
        nested = json!({"Composite":[nested]});
    }
    let deep = json!({"version":1,"action":99,"command":{"id":"edit.guardedPatch","patch":nested}}).to_string();
    assert!(decode_entry(&deep).is_err(), "deep guarded patches must respect the live JSON depth limit");

    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 60);
    assert_eq!(value(&alice), Value::text("original"), "a well-formed guarded value behaves like guarded cell.set");
    let before = value(&alice);
    let direct = alice.editor_mut().session_mut().execute("internal.coauthorGuardedPatch", json!({"patch":{}}));
    assert!(direct.is_err(), "ordinary command dispatch cannot supply an internal guarded application");
    assert_eq!(value(&alice), before);

    let edit = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"yearless-current"}), 70);
    order_and_deliver(&mut server, &edit, &mut alice, &mut bob, 80);
    let undo = alice.undo(90).expect("second guarded undo submits");
    let mut forged = server.one_proposal(&alice_place, &undo);
    let mut payload: serde_json::Value = serde_json::from_str(&forged.payload).expect("guarded proposal is JSON");
    payload["command"]["patch"]["Cells"][0]["restore"] = json!({"input":"6/1","formula":false,"references":[]});
    forged.payload = serde_json::to_string(&payload).expect("forged canonicality probe encodes");
    let ordered = server.order(&forged, 1_795_219_200_090, 43);
    let crashed = server.deliver(&mut bob, &ordered, 100);
    assert!(
        crashed.events.iter().any(|event| matches!(event, Event::Applied { effect: Effect::NoEffect { .. }, .. })),
        "yearless forged literals are refused during application"
    );
    assert_eq!(value(&bob), Value::text("yearless-current"));
}

#[test]
fn guarded_style_patches_obey_sheet_protection() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 20);

    let format = propose(&server, &mut alice, &alice_place, "home.fontSize", json!({"sheet":"Sheet1","range":"A1","size":18}), 30);
    order_and_deliver(&mut server, &format, &mut alice, &mut bob, 40);
    protect_sheet(&mut alice, SheetProtection::default());
    protect_sheet(&mut bob, SheetProtection::default());
    let before = alice.editor().session().doc().expect("document exists").wb.clone();
    let undo = alice.undo(50).expect("format undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 60);
    assert_eq!(alice.editor().session().doc().expect("document exists").wb, before);
    assert_eq!(style(&alice).font.size, 18.0, "formatting forbidden by protection is left untouched");

    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 70);
    server.join(&mut bob, 80);
    let hide = propose(
        &server,
        &mut alice,
        &alice_place,
        "home.formatCells",
        json!({"sheet":"Sheet1","range":"A1","style":{"protection":{"hidden":true}}}),
        90,
    );
    order_and_deliver(&mut server, &hide, &mut alice, &mut bob, 100);
    protect_sheet(&mut alice, SheetProtection { format_cells: true, ..SheetProtection::default() });
    protect_sheet(&mut bob, SheetProtection { format_cells: true, ..SheetProtection::default() });
    let before = alice.editor().session().doc().expect("document exists").wb.clone();
    let undo = alice.undo(110).expect("hidden flag undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 120);
    assert_eq!(alice.editor().session().doc().expect("document exists").wb, before);
    assert!(style(&alice).protection.hidden, "a guarded patch cannot reveal a hidden formula");
}

#[test]
fn guarded_content_patch_obeys_locked_cell_protection() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 20);
    let edit = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"alice"}), 30);
    order_and_deliver(&mut server, &edit, &mut alice, &mut bob, 40);
    protect_sheet(&mut alice, SheetProtection::default());
    protect_sheet(&mut bob, SheetProtection::default());
    let before = alice.editor().session().doc().expect("document exists").wb.clone();
    let undo = alice.undo(50).expect("content undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 60);
    assert_eq!(alice.editor().session().doc().expect("document exists").wb, before);
    assert_eq!(value(&alice), Value::text("alice"));
}

#[test]
fn python_cell_does_not_match_an_empty_literal_guard() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 20);
    let edit = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"alice"}), 30);
    order_and_deliver(&mut server, &edit, &mut alice, &mut bob, 40);
    let undo = alice.undo(50).expect("content undo submits");
    let mut forged = server.one_proposal(&alice_place, &undo);
    let mut payload: serde_json::Value = serde_json::from_str(&forged.payload).expect("guarded proposal is JSON");
    payload["command"]["patch"]["Cells"][0]["expected"] = json!({"input":"","formula":false,"references":[]});
    forged.payload = serde_json::to_string(&payload).expect("forged empty guard encodes");
    bob.editor_mut()
        .session_mut()
        .execute("python.set", json!({"sheet":"Sheet1","cell":"A1","code":"secret = 1\nsecret"}))
        .expect("later Python cell is set");
    let ordered = server.order(&forged, 1_795_219_200_050, 43);
    server.deliver(&mut bob, &ordered, 60);
    let cell = bob
        .editor()
        .session()
        .doc()
        .expect("document exists")
        .wb
        .sheet(0)
        .expect("sheet exists")
        .cell(CellRef::new(0, 0))
        .expect("Python cell remains");
    assert_eq!(cell.py.as_ref().map(|python| python.code.as_str()), Some("secret = 1\nsecret"));
}

#[test]
fn guarded_line_style_cannot_unlock_a_protected_column() {
    let make_client = || {
        let mut session = initial_session();
        session
            .execute("home.formatCells", json!({"sheet":"Sheet1","range":"A:A","style":{"protection":{"locked":false}}}))
            .expect("column starts unlocked");
        GridCoauthor::new(live_config(), GridLiveEditor::new(session).expect("editor builds"))
    };
    let mut server = DeterministicServer::new();
    let mut alice = make_client();
    let mut bob = make_client();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 20);
    let lock = propose(
        &server,
        &mut alice,
        &alice_place,
        "home.formatCells",
        json!({"sheet":"Sheet1","range":"A:A","style":{"protection":{"locked":true}}}),
        30,
    );
    order_and_deliver(&mut server, &lock, &mut alice, &mut bob, 40);
    protect_sheet(&mut alice, SheetProtection { format_cells: true, ..SheetProtection::default() });
    protect_sheet(&mut bob, SheetProtection { format_cells: true, ..SheetProtection::default() });
    let before = alice.editor().session().doc().expect("document exists").wb.clone();
    let undo = alice.undo(50).expect("column lock undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 60);
    assert_eq!(alice.editor().session().doc().expect("document exists").wb, before);
    assert!(style(&alice).protection.locked, "the protected column stays locked");
    assert!(
        alice.editor_mut().session_mut().execute("cell.set", json!({"sheet":"Sheet1","cell":"A2","input":"forged"})).is_err(),
        "a later cell edit cannot exploit a guarded line-style unlock"
    );
}

#[test]
fn guarded_structural_patches_obey_protection_gates() {
    for (command, params) in [
        ("home.insertRows", json!({"sheet":"Sheet1","rows":"1:1"})),
        ("home.deleteRows", json!({"sheet":"Sheet1","rows":"1:1"})),
        ("home.insertColumns", json!({"sheet":"Sheet1","cols":"A:A"})),
        ("home.deleteColumns", json!({"sheet":"Sheet1","cols":"A:A"})),
    ] {
        let mut server = DeterministicServer::new();
        let mut alice = client();
        let mut bob = client();
        let alice_place = server.join(&mut alice, 10);
        server.join(&mut bob, 20);
        let change = propose(&server, &mut alice, &alice_place, command, params, 30);
        order_and_deliver(&mut server, &change, &mut alice, &mut bob, 40);
        protect_sheet(&mut alice, SheetProtection::default());
        protect_sheet(&mut bob, SheetProtection::default());
        let before = alice.editor().session().doc().expect("document exists").wb.clone();
        let undo = alice.undo(50).expect("structural undo submits");
        let proposal = server.one_proposal(&alice_place, &undo);
        order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 60);
        assert_eq!(alice.editor().session().doc().expect("document exists").wb, before, "{command} reverse obeys protection");
    }

    for (command, params) in [("home.insertSheet", json!({"name":"Added"})), ("sheet.rename", json!({"sheet":"Sheet1","name":"Budget"}))] {
        let mut server = DeterministicServer::new();
        let mut alice = client();
        let mut bob = client();
        let alice_place = server.join(&mut alice, 10);
        server.join(&mut bob, 20);
        let change = propose(&server, &mut alice, &alice_place, command, params, 30);
        order_and_deliver(&mut server, &change, &mut alice, &mut bob, 40);
        protect_workbook_structure(&mut alice);
        protect_workbook_structure(&mut bob);
        let before = alice.editor().session().doc().expect("document exists").wb.clone();
        let undo = alice.undo(50).expect("sheet undo submits");
        let proposal = server.one_proposal(&alice_place, &undo);
        order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 60);
        assert_eq!(alice.editor().session().doc().expect("document exists").wb, before, "{command} reverse obeys structure protection");
    }

    let make_client = || {
        let session = session_with_sheets(&["Deleted", "Other"]);
        GridCoauthor::new(live_config(), GridLiveEditor::new(session).expect("editor builds"))
    };
    let mut server = DeterministicServer::new();
    let mut alice = make_client();
    let mut bob = make_client();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 20);
    let deletion = propose(&server, &mut alice, &alice_place, "home.deleteSheet", json!({"sheet":"Deleted"}), 30);
    order_and_deliver(&mut server, &deletion, &mut alice, &mut bob, 40);
    protect_workbook_structure(&mut alice);
    protect_workbook_structure(&mut bob);
    let before = alice.editor().session().doc().expect("document exists").wb.clone();
    let undo = alice.undo(50).expect("whole-action undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 60);
    assert_eq!(alice.editor().session().doc().expect("document exists").wb, before, "WholeAction obeys workbook structure protection");
}

#[test]
fn duplicate_guarded_line_leaves_are_refused_quickly_without_changes() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 20);
    let format = propose(&server, &mut alice, &alice_place, "home.fillColor", json!({"sheet":"Sheet1","range":"A:A","color":"#123456"}), 30);
    order_and_deliver(&mut server, &format, &mut alice, &mut bob, 40);
    let undo = alice.undo(50).expect("column format undo submits");
    let mut forged = server.one_proposal(&alice_place, &undo);
    let mut payload: serde_json::Value = serde_json::from_str(&forged.payload).expect("guarded proposal is JSON");
    let leaf = payload["command"]["patch"]["Styles"][0].clone();
    payload["command"]["patch"]["Styles"] = serde_json::Value::Array(vec![leaf; 500]);
    forged.payload = serde_json::to_string(&payload).expect("forged duplicate guards encode");
    assert!(forged.payload.len() <= MAX_PAYLOAD_BYTES, "the hostile entry stays within the wire limit");
    let before = bob.editor().session().doc().expect("document exists").wb.clone();
    let ordered = server.order(&forged, 1_795_219_200_050, 43);
    let started = std::time::SystemTime::UNIX_EPOCH.elapsed().unwrap_or_default();
    let step = server.deliver(&mut bob, &ordered, 60);
    let elapsed = std::time::SystemTime::UNIX_EPOCH.elapsed().unwrap_or_default().saturating_sub(started);
    assert!(elapsed < std::time::Duration::from_secs(1), "duplicate guards are rejected before repeated line scans");
    assert!(step.events.iter().any(|event| matches!(event, Event::Applied { effect: Effect::NoEffect { .. }, .. })));
    assert_eq!(bob.editor().session().doc().expect("document exists").wb, before);
}

#[test]
fn duplicate_guards_inside_a_structural_patch_are_refused_without_changes() {
    let mut server = DeterministicServer::new();
    let mut alice = two_sheet_structural_client();
    let mut bob = two_sheet_structural_client();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 20);
    let deletion = propose(&server, &mut alice, &alice_place, "home.deleteRows", json!({"sheet":"Sheet1","rows":"2:2"}), 30);
    order_and_deliver(&mut server, &deletion, &mut alice, &mut bob, 40);
    let undo = alice.undo(50).expect("row delete undo submits");
    let mut forged = server.one_proposal(&alice_place, &undo);
    let mut payload: serde_json::Value = serde_json::from_str(&forged.payload).expect("structural guard is JSON");
    let first = payload["command"]["patch"]["Structural"]["state"]["cells"][0].clone();
    payload["command"]["patch"]["Structural"]["state"]["cells"].as_array_mut().expect("structural cell guards are an array").push(first);
    forged.payload = serde_json::to_string(&payload).expect("duplicate structural guard encodes");
    let before = bob.editor().session().doc().expect("document exists").wb.clone();
    let ordered = server.order(&forged, 1_795_219_200_050, 43);
    let step = server.deliver(&mut bob, &ordered, 60);
    assert!(step.events.iter().any(|event| matches!(event, Event::Applied { effect: Effect::NoEffect { .. }, .. })));
    assert_eq!(bob.editor().session().doc().expect("document exists").wb, before);
}

#[test]
fn pending_undo_and_redo_survive_rollback_and_rerun() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let edit = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"alice"}), 30);
    order_and_deliver(&mut server, &edit, &mut alice, &mut bob, 40);

    let undo_step = alice.undo(50).expect("undo submits");
    let undo = server.one_proposal(&alice_place, &undo_step);
    let bob_bold = propose(&server, &mut bob, &bob_place, "home.bold", json!({"sheet":"Sheet1","range":"A1","on":true}), 51);
    let ordered_bold = server.order(&bob_bold, 1_795_219_200_051, 42);
    server.deliver(&mut alice, &ordered_bold, 60);
    server.deliver(&mut bob, &ordered_bold, 61);
    let ordered_undo = server.order(&undo, 1_795_219_200_052, 43);
    server.deliver(&mut alice, &ordered_undo, 70);
    server.deliver(&mut bob, &ordered_undo, 71);
    assert_eq!(value(&alice), Value::text("original"));
    assert!(style(&alice).font.bold);
    assert_eq!((alice.history().undo_len(), alice.history().redo_len()), (0, 1));

    let redo_step = alice.redo(80).expect("redo submits");
    let redo = server.one_proposal(&alice_place, &redo_step);
    let bob_fill = propose(&server, &mut bob, &bob_place, "home.fillColor", json!({"sheet":"Sheet1","range":"A1","color":"#123456"}), 81);
    let ordered_fill = server.order(&bob_fill, 1_795_219_200_081, 44);
    server.deliver(&mut alice, &ordered_fill, 90);
    server.deliver(&mut bob, &ordered_fill, 91);
    let ordered_redo = server.order(&redo, 1_795_219_200_082, 45);
    server.deliver(&mut alice, &ordered_redo, 100);
    server.deliver(&mut bob, &ordered_redo, 101);

    assert_eq!(value(&alice), Value::text("alice"));
    assert!(style(&alice).font.bold);
    assert_ne!(style(&alice).fill, Style::default().fill);
    assert_eq!((alice.history().undo_len(), alice.history().redo_len()), (1, 0));
}

#[test]
fn same_instance_reconnect_keeps_history_when_the_place_is_kept() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let alice_place = server.join(&mut alice, 10);
    let mut peer = client();
    server.join(&mut peer, 20);
    let edit = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"alice"}), 30);
    order_and_deliver(&mut server, &edit, &mut alice, &mut peer, 40);

    alice.disconnected(50);
    assert_eq!(alice.connected(60).len(), 1, "reconnect sends a fresh live join");
    alice
        .receive(
            &json!({
                "type":"live_joined","session":"00000000-0000-0000-0000-00000000005e",
                "participant":alice_place.participant.0,"place":{"is":"kept"},"can_edit":true,"heartbeat_ms":60_000,"people":[]
            })
            .to_string(),
            61,
        )
        .expect("kept place is accepted");
    alice.receive(&json!({"type":"catchup","base":0,"revision":null,"from":1,"head":1}).to_string(), 62).expect("same-base catch-up is accepted");

    assert_eq!(alice.history().undo_len(), 1, "a true same-instance resume keeps personal history");
}

#[test]
fn replacement_base_and_returned_place_clear_personal_history() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let alice_place = server.join(&mut alice, 10);
    let mut peer = client();
    server.join(&mut peer, 20);
    let edit = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"alice"}), 30);
    let ordered_edit = order_and_deliver(&mut server, &edit, &mut alice, &mut peer, 40);
    let undo = alice.undo(41).expect("pending undo starts");
    let pending_undo = server.one_proposal(&alice_place, &undo);

    alice.disconnected(50);
    alice.connected(60);
    alice
        .receive(
            &json!({
                "type":"live_joined","session":"00000000-0000-0000-0000-00000000005e",
                "participant":alice_place.participant.0,"place":{"is":"kept"},"can_edit":true,"heartbeat_ms":60_000,"people":[]
            })
            .to_string(),
            61,
        )
        .expect("kept place is accepted");
    let load = alice
        .receive(&json!({"type":"catchup","base":0,"revision":"replacement","from":0,"head":1}).to_string(), 62)
        .expect("replacement catch-up is accepted");
    assert!(load.events.iter().any(|event| matches!(event, Event::LoadBase { .. })));
    assert_eq!((alice.history().undo_len(), alice.history().redo_len()), (0, 0));
    assert!(alice.last_undo_report().is_none());

    alice.base_ready(63).expect("replacement base is ready");
    server.deliver(&mut alice, &ordered_edit, 64);
    let new_edit = propose(&server, &mut alice, &alice_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"after reconnect"}), 65);
    let ordered_old_undo = server.order(&pending_undo, 1_795_219_200_065, 47);
    server.deliver(&mut alice, &ordered_old_undo, 66);
    assert_eq!(alice.history().undo_len(), 0, "the old acknowledgement consumes only its discarded history slot");
    let ordered_new_edit = server.order(&new_edit, 1_795_219_200_066, 48);
    server.deliver(&mut alice, &ordered_new_edit, 67);
    assert_eq!(alice.history().undo_len(), 1, "the post-reconnect action settles independently");

    let mut returned_server = DeterministicServer::new();
    let mut returned = client();
    let returned_place = returned_server.join(&mut returned, 100);
    let edit = propose(&returned_server, &mut returned, &returned_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"returned"}), 110);
    let ordered = returned_server.order(&edit, 1_795_219_200_110, 46);
    returned_server.deliver(&mut returned, &ordered, 120);
    assert_eq!(returned.history().undo_len(), 1);
    let pending =
        propose(&returned_server, &mut returned, &returned_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"pending return"}), 125);
    returned.disconnected(130);
    returned.connected(140);
    let step = returned
        .receive(
            &json!({
                "type":"live_joined","session":"00000000-0000-0000-0000-00000000005e",
                "participant":99,"place":{"is":"returning","previous":returned_place.participant.0},
                "can_edit":false,"heartbeat_ms":60_000,"people":[]
            })
            .to_string(),
            141,
        )
        .expect("returned place is accepted");
    assert!(step.events.iter().any(|event| matches!(event, Event::Returned { .. })));
    assert!(step.events.iter().any(|event| matches!(event, Event::DroppedPending { count: 1 })));
    assert_eq!((returned.history().undo_len(), returned.history().redo_len()), (0, 0));
    assert!(returned.last_undo_report().is_none());
    returned.receive(&json!({"type":"catchup","base":0,"revision":null,"from":1,"head":1}).to_string(), 142).expect("returned catch-up is accepted");
    returned.receive(&json!({"type":"access","can_edit":true}).to_string(), 143).expect("edit access returns");
    let fresh = propose(
        &returned_server,
        &mut returned,
        &ClientPlace { client: returned_place.client, participant: protocol::live::ParticipantNo(99) },
        "cell.set",
        json!({"sheet":"Sheet1","cell":"A1","input":"fresh return"}),
        144,
    );
    let ordered_fresh = returned_server.order(&fresh, 1_795_219_200_144, 49);
    returned_server.deliver(&mut returned, &ordered_fresh, 145);
    assert_eq!(returned.history().undo_len(), 1, "the co-emitted drop consumes the discarded settlement slot");
    let _ = pending;
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

fn sheet_value(client: &GridCoauthor, sheet_name: &str, cell: &str) -> Value {
    let document = client.editor().session().doc().expect("document exists");
    let sheet_index = document.wb.sheet_index(sheet_name).expect("named sheet exists");
    let at = CellRef::parse(cell).expect("test cell parses");
    document.wb.sheet(sheet_index).expect("sheet exists").value(at)
}

fn sheet_formula(client: &GridCoauthor, sheet_name: &str, cell: &str) -> Option<String> {
    let document = client.editor().session().doc().expect("document exists");
    let sheet_index = document.wb.sheet_index(sheet_name).expect("named sheet exists");
    let at = CellRef::parse(cell).expect("test cell parses");
    document.wb.sheet(sheet_index).expect("sheet exists").cell(at).and_then(|cell| cell.formula.as_ref()).map(|formula| formula.text.clone())
}

fn sheet_style(client: &GridCoauthor, sheet_name: &str, cell: &str) -> Style {
    let document = client.editor().session().doc().expect("document exists");
    let sheet_index = document.wb.sheet_index(sheet_name).expect("named sheet exists");
    let at = CellRef::parse(cell).expect("test cell parses");
    let sheet = document.wb.sheet(sheet_index).expect("sheet exists");
    document.wb.styles.get(sheet.style_id(at)).clone()
}

fn two_sheet_structural_client() -> GridCoauthor {
    let mut session = session_with_sheets(&["Sheet1", "Other"]);
    session.execute("cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"7"})).expect("source value is set");
    session.execute("cell.set", json!({"sheet":"Sheet1","cell":"A2","input":"deleted"})).expect("deleted value is set");
    session.execute("cell.set", json!({"sheet":"Sheet1","cell":"B1","input":"=A1"})).expect("formula is set");
    session.execute("home.fillColor", json!({"sheet":"Sheet1","range":"A:A","color":"#123456"})).expect("column format is set");
    GridCoauthor::new(live_config(), GridLiveEditor::new(session).expect("editor builds"))
}

#[test]
fn row_delete_undo_restores_the_deleted_identities_and_cells() {
    let mut server = DeterministicServer::new();
    let mut alice = two_sheet_structural_client();
    let mut bob = two_sheet_structural_client();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 20);
    let sheet_id = alice.editor().ids().sheet_id(0).expect("sheet identity exists");
    let deleted_row = alice.editor().ids().by_id[&sheet_id].rows.id_at(1).expect("row identity exists");

    let deletion = propose(&server, &mut alice, &alice_place, "home.deleteRows", json!({"sheet":"Sheet1","rows":"2:2"}), 30);
    order_and_deliver(&mut server, &deletion, &mut alice, &mut bob, 40);
    assert_eq!(alice.history().undo_len(), 1, "the structural action is recorded");

    let undo = alice.undo(50).expect("row delete undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 60);

    assert_eq!(sheet_value(&alice, "Sheet1", "A2"), Value::text("deleted"));
    assert_eq!(sheet_value(&bob, "Sheet1", "A2"), Value::text("deleted"));
    assert_eq!(alice.editor().ids().by_id[&sheet_id].rows.id_at(1), Some(deleted_row));
    assert_eq!(bob.editor().ids().by_id[&sheet_id].rows.id_at(1), Some(deleted_row));
}

#[test]
fn inserted_row_changed_by_another_person_is_not_removed() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let insertion = propose(&server, &mut alice, &alice_place, "home.insertRows", json!({"sheet":"Sheet1","rows":"1:1"}), 30);
    order_and_deliver(&mut server, &insertion, &mut alice, &mut bob, 40);
    let edit = propose(&server, &mut bob, &bob_place, "cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"bob"}), 50);
    order_and_deliver(&mut server, &edit, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("insert undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert_eq!(sheet_value(&alice, "Sheet1", "A1"), Value::text("bob"));
    assert_eq!(sheet_value(&alice, "Sheet1", "A2"), Value::text("original"));
    let report = alice.last_undo_report().expect("guard refusal is reported");
    assert_eq!((report.restored, report.left), (0, 1));
}

#[test]
fn column_delete_undo_restores_formulas_and_formats() {
    let mut server = DeterministicServer::new();
    let mut alice = two_sheet_structural_client();
    let mut bob = two_sheet_structural_client();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 20);

    let deletion = propose(&server, &mut alice, &alice_place, "home.deleteColumns", json!({"sheet":"Sheet1","cols":"A:A"}), 30);
    order_and_deliver(&mut server, &deletion, &mut alice, &mut bob, 40);
    let undo = alice.undo(50).expect("column delete undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 60);

    assert_eq!(sheet_value(&alice, "Sheet1", "A1"), Value::Number(7.0));
    assert_eq!(sheet_formula(&alice, "Sheet1", "B1").as_deref(), Some("A1"));
    assert_ne!(sheet_style(&alice, "Sheet1", "A1").fill, Style::default().fill);
    assert_eq!(sheet_formula(&bob, "Sheet1", "B1").as_deref(), Some("A1"));
}

#[test]
fn sheet_rename_undo_keeps_later_cell_edits() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let rename = propose(&server, &mut alice, &alice_place, "sheet.rename", json!({"sheet":"Sheet1","name":"Budget"}), 30);
    order_and_deliver(&mut server, &rename, &mut alice, &mut bob, 40);
    let edit = propose(&server, &mut bob, &bob_place, "cell.set", json!({"sheet":"Budget","cell":"B2","input":"later"}), 50);
    order_and_deliver(&mut server, &edit, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("rename undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert_eq!(sheet_value(&alice, "Sheet1", "B2"), Value::text("later"));
    assert_eq!(sheet_value(&bob, "Sheet1", "B2"), Value::text("later"));
    assert!(alice.editor().session().doc().expect("document exists").wb.sheet_index("Budget").is_none());
}

#[test]
fn sheet_add_undo_refuses_after_another_person_edits_it() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let add = propose(&server, &mut alice, &alice_place, "home.insertSheet", json!({"name":"Added"}), 30);
    order_and_deliver(&mut server, &add, &mut alice, &mut bob, 40);
    let edit = propose(&server, &mut bob, &bob_place, "cell.set", json!({"sheet":"Added","cell":"A1","input":"bob"}), 50);
    order_and_deliver(&mut server, &edit, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("sheet add undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert_eq!(sheet_value(&alice, "Added", "A1"), Value::text("bob"));
    assert_eq!(sheet_value(&bob, "Added", "A1"), Value::text("bob"));
    let report = alice.last_undo_report().expect("guard refusal is reported");
    assert_eq!((report.restored, report.left), (0, 1));
}

fn delete_row_with_text(text_len: usize, extra_cell: bool) -> (DeterministicServer, ClientPlace, GridCoauthor, GridCoauthor) {
    let make_session = || {
        let mut session = session_with_sheets(&["Sheet1"]);
        {
            let document = session.doc_mut().expect("large document exists");
            let workbook = Arc::make_mut(&mut document.wb);
            let sheet = Arc::make_mut(workbook.sheets.get_mut(0).expect("large sheet exists"));
            sheet.cells.set(CellRef::new(0, 0), gridcraft_model::Cell::value(Value::text("x".repeat(text_len))));
            if extra_cell {
                sheet.cells.set(CellRef::new(0, 1), gridcraft_model::Cell::value(Value::text("y")));
            }
        }
        session
    };
    let mut alice = GridCoauthor::new(live_config(), GridLiveEditor::new(make_session()).expect("editor builds"));
    let mut bob = GridCoauthor::new(live_config(), GridLiveEditor::new(make_session()).expect("editor builds"));
    let mut server = DeterministicServer::new();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 20);
    let deletion = propose(&server, &mut alice, &alice_place, "home.deleteRows", json!({"sheet":"Sheet1","rows":"1:1"}), 30);
    order_and_deliver(&mut server, &deletion, &mut alice, &mut bob, 40);
    (server, alice_place, alice, bob)
}

#[test]
fn sort_large_paste_and_sheet_delete_require_untouched_guard() {
    let make_session = || {
        let mut session = session_with_sheets(&["Source", "Summary"]);
        session.execute("cell.set", json!({"sheet":"Source","cell":"A1","input":"7"})).expect("source is set");
        session.execute("cell.set", json!({"sheet":"Summary","cell":"A1","input":"=Source!A1"})).expect("dependent formula is set");
        session
    };
    let mut alice = GridCoauthor::new(live_config(), GridLiveEditor::new(make_session()).expect("editor builds"));
    let mut bob = GridCoauthor::new(live_config(), GridLiveEditor::new(make_session()).expect("editor builds"));
    let mut server = DeterministicServer::new();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let deletion = propose(&server, &mut alice, &alice_place, "home.deleteSheet", json!({"sheet":"Source"}), 30);
    order_and_deliver(&mut server, &deletion, &mut alice, &mut bob, 40);
    let edit = propose(&server, &mut bob, &bob_place, "cell.set", json!({"sheet":"Summary","cell":"A1","input":"later"}), 50);
    order_and_deliver(&mut server, &edit, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("sheet delete undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    let decoded = decode_entry(&proposal.payload).expect("whole-action guard decodes");
    let whole_action = match decoded.command {
        SharedCommand::Guarded { patch: GuardedPatch::Structural(structural) } => matches!(*structural, GuardedStructural::WholeAction { .. }),
        _ => false,
    };
    assert!(whole_action, "sheet delete uses the reusable all-or-nothing guard");
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);
    assert!(alice.editor().session().doc().expect("document exists").wb.sheet_index("Source").is_none());
    assert_eq!(sheet_value(&alice, "Summary", "A1"), Value::text("later"));

    let (probe_server, probe_place, mut probe, _probe_peer) = delete_row_with_text(1, false);
    let probe_undo = probe.undo(90).expect("the payload overhead probe submits");
    let probe_payload = probe_server.one_proposal(&probe_place, &probe_undo).payload.len();
    let payload_overhead = probe_payload.checked_sub(1).expect("the payload includes the probe text");
    let exact_limit_text =
        MAX_PAYLOAD_BYTES.checked_sub(payload_overhead).and_then(|length| length.checked_add(1)).expect("the wire limit exceeds structural overhead");

    let (mut below_server, below_place, mut below, mut below_peer) = delete_row_with_text(exact_limit_text, false);
    let below_undo = below.undo(100).expect("a structural reverse below the wire limit submits");
    let below_proposal = below_server.one_proposal(&below_place, &below_undo);
    assert_eq!(below_proposal.payload.len(), MAX_PAYLOAD_BYTES - 1, "the accepted reverse sits immediately below the wire limit");
    order_and_deliver(&mut below_server, &below_proposal, &mut below, &mut below_peer, 110);
    assert!(matches!(sheet_value(&below, "Sheet1", "A1"), Value::Text(_)));

    let (above_server, _above_place, mut above, _above_peer) = delete_row_with_text(exact_limit_text, true);
    let before = above.editor().session().doc().expect("document exists").wb.clone();
    let error = above.undo(120).expect_err("a structural reverse above the wire limit is refused").to_string();
    assert!(error.contains("This change is too large to undo while sharing"), "{error}");
    assert_eq!(above.editor().session().doc().expect("document exists").wb, before);
    assert_eq!((above.history().undo_len(), above.history().redo_len()), (1, 0));
    let _ = above_server;
}

#[test]
fn row_delete_undo_refuses_after_the_tail_filler_line_changes() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let deletion = propose(&server, &mut alice, &alice_place, "home.deleteRows", json!({"sheet":"Sheet1","rows":"1:1"}), 30);
    order_and_deliver(&mut server, &deletion, &mut alice, &mut bob, 40);
    let tail = format!("{MAX_ROWS}:{MAX_ROWS}");
    let format = propose(&server, &mut bob, &bob_place, "home.fillColor", json!({"sheet":"Sheet1","range":tail,"color":"#123456"}), 50);
    order_and_deliver(&mut server, &format, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("row delete undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert_eq!(sheet_value(&alice, "Sheet1", "A1"), Value::Empty, "the deletion stays in place");
    let report = alice.last_undo_report().expect("guard refusal is reported");
    assert_eq!((report.restored, report.left), (0, 1));
}

#[test]
fn row_delete_undo_refuses_after_another_delete_replaces_the_tail_filler() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let alice_delete = propose(&server, &mut alice, &alice_place, "home.deleteRows", json!({"sheet":"Sheet1","rows":"1:1"}), 30);
    order_and_deliver(&mut server, &alice_delete, &mut alice, &mut bob, 40);
    let bob_delete = propose(&server, &mut bob, &bob_place, "home.deleteRows", json!({"sheet":"Sheet1","rows":"2:2"}), 50);
    order_and_deliver(&mut server, &bob_delete, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("row delete undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    let report = alice.last_undo_report().expect("guard refusal is reported");
    assert_eq!((report.restored, report.left), (0, 1));
    assert_eq!(alice.editor().ids(), bob.editor().ids(), "refusal keeps workbook identities converged");
}

#[test]
fn sheet_delete_undo_refuses_after_a_surviving_sheet_changes_structure() {
    let make_session = || {
        let mut session = session_with_sheets(&["Deleted", "Other"]);
        {
            let document = session.doc_mut().expect("document exists");
            Arc::make_mut(&mut document.wb).sheets.get_mut(0).map(Arc::make_mut).expect("deleted sheet exists").file_id = 42;
        }
        session.execute("cell.set", json!({"sheet":"Deleted","cell":"A1","input":"=Other!A1"})).expect("formula is set");
        session
    };
    let mut alice = GridCoauthor::new(live_config(), GridLiveEditor::new(make_session()).expect("editor builds"));
    let mut bob = GridCoauthor::new(live_config(), GridLiveEditor::new(make_session()).expect("editor builds"));
    let mut server = DeterministicServer::new();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let deletion = propose(&server, &mut alice, &alice_place, "home.deleteSheet", json!({"sheet":"Deleted"}), 30);
    order_and_deliver(&mut server, &deletion, &mut alice, &mut bob, 40);
    let insertion = propose(&server, &mut bob, &bob_place, "home.insertRows", json!({"sheet":"Other","rows":"1:1"}), 50);
    order_and_deliver(&mut server, &insertion, &mut bob, &mut alice, 60);

    let undo = alice.undo(70).expect("sheet delete undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 80);

    assert!(alice.editor().session().doc().expect("document exists").wb.sheet_index("Deleted").is_none());
    assert!(bob.editor().session().doc().expect("document exists").wb.sheet_index("Deleted").is_none());
    let report = alice.last_undo_report().expect("guard refusal is reported");
    assert_eq!((report.restored, report.left), (0, 1));
}

#[test]
fn sheet_delete_undo_preserves_the_file_sheet_identity() {
    let make_session = || {
        let mut session = session_with_sheets(&["Deleted", "Other"]);
        let document = session.doc_mut().expect("document exists");
        Arc::make_mut(&mut document.wb).sheets.get_mut(0).map(Arc::make_mut).expect("deleted sheet exists").file_id = 42;
        session
    };
    let mut alice = GridCoauthor::new(live_config(), GridLiveEditor::new(make_session()).expect("editor builds"));
    let mut bob = GridCoauthor::new(live_config(), GridLiveEditor::new(make_session()).expect("editor builds"));
    let mut server = DeterministicServer::new();
    let alice_place = server.join(&mut alice, 10);
    server.join(&mut bob, 20);

    let deletion = propose(&server, &mut alice, &alice_place, "home.deleteSheet", json!({"sheet":"Deleted"}), 30);
    order_and_deliver(&mut server, &deletion, &mut alice, &mut bob, 40);
    let undo = alice.undo(50).expect("sheet delete undo submits");
    let proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &proposal, &mut alice, &mut bob, 60);

    let restored = alice.editor().session().doc().expect("document exists").wb.sheet(0).expect("restored sheet exists");
    assert_eq!(restored.file_id, 42, "XLSX sheet identity is restored");
}

#[test]
fn redo_reuses_the_same_guards_in_reverse() {
    let mut server = DeterministicServer::new();
    let mut alice = client();
    let mut bob = client();
    let alice_place = server.join(&mut alice, 10);
    let bob_place = server.join(&mut bob, 20);

    let rename = propose(&server, &mut alice, &alice_place, "sheet.rename", json!({"sheet":"Sheet1","name":"Budget"}), 30);
    order_and_deliver(&mut server, &rename, &mut alice, &mut bob, 40);
    let undo = alice.undo(50).expect("rename undo submits");
    let undo_proposal = server.one_proposal(&alice_place, &undo);
    order_and_deliver(&mut server, &undo_proposal, &mut alice, &mut bob, 60);

    let other_rename = propose(&server, &mut bob, &bob_place, "sheet.rename", json!({"sheet":"Sheet1","name":"OtherName"}), 70);
    order_and_deliver(&mut server, &other_rename, &mut bob, &mut alice, 80);
    let redo = alice.redo(90).expect("rename redo submits");
    let redo_proposal = server.one_proposal(&alice_place, &redo);
    order_and_deliver(&mut server, &redo_proposal, &mut alice, &mut bob, 100);

    assert!(alice.editor().session().doc().expect("document exists").wb.sheet_index("OtherName").is_some());
    assert!(alice.editor().session().doc().expect("document exists").wb.sheet_index("Budget").is_none());
    let report = alice.last_undo_report().expect("redo guard refusal is reported");
    assert_eq!((report.restored, report.left), (0, 1));
}
