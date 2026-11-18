use collab_core::ids::Seq;
use collab_live::client::{Crash, Effect, EntryView, Event, LiveClient, LiveConfig, LiveEditor, Mode, Outcome, Stamp};
use gridcraft_core::{CellRef, Value};
use gridcraft_engine::catalog::CATALOG;
use gridcraft_engine::coauthor::{
    CoauthorError, GRIDCRAFT_LIVE_FORMAT, GridLiveEditor, GridSnapshot, IdentityPosition, IdentityStructural, SharedCommand, TargetGone, WorkbookIds,
    decode_entry, encode_command, is_shared_command, resolve_command,
};
use gridcraft_engine::fingerprint::{DocId, fingerprint};
use gridcraft_engine::{DocState, Session, Trust};
use gridcraft_formula::{parse, print};
use gridcraft_model::{Sheet, Workbook};
use protocol::live::MAX_PAYLOAD_BYTES;
use protocol::live::ParticipantNo;
use serde_json::{Value as Json, json};
use std::sync::Arc;

fn entry(command: Json) -> String {
    json!({"version": 1, "action": 1, "command": command}).to_string()
}

fn cell_identity(row: u32, col: u32) -> Json {
    json!({"sheet":[0,0],"row":[0,row],"col":[0,col]})
}

fn range_identity(a: (u32, u32), b: (u32, u32), kind: &str) -> Json {
    json!({"sheet":[0,0],"start":cell_identity(a.0,a.1),"end":cell_identity(b.0,b.1),"kind":kind})
}

fn session_with_sheets(names: &[&str]) -> Session {
    let mut workbook = Workbook::new();
    workbook.sheets.clear();
    workbook.sheets.extend(names.iter().map(|name| Arc::new(Sheet::new(*name))));
    let mut session = Session::new();
    session.add_document(DocState::new(workbook, None, "Book1".into(), Trust::Local));
    session
}

fn explicit_command_session() -> Session {
    let mut session = session_with_sheets(&["Data", "View"]);
    session
        .execute("range.setValues", json!({"sheet":"Data","range":"A1:C4","values":[["Name","Group","Value"],["A","x",1],["B","y",2],["C","x",3]]}))
        .expect("filter data is entered");
    session.execute("home.fontColor", json!({"sheet":"View","range":"A1","color":{"theme":4,"tint":0.0}})).expect("theme active style is set");
    session.execute("home.fontColor", json!({"sheet":"View","range":"B1","color":"#123456"})).expect("RGB active style is set");
    session.execute("home.fontSize", json!({"sheet":"View","range":"A1","size":8})).expect("small active style is set");
    session.execute("home.fontSize", json!({"sheet":"View","range":"B1","size":24})).expect("large active style is set");
    session.execute("home.underline", json!({"sheet":"View","range":"A1","on":true,"style":"single"})).expect("underlined active style is set");
    session.execute("home.fontColor", json!({"sheet":"Data","range":"A1","color":"#654321"})).expect("RGB target style is set");
    session.execute("home.fontColor", json!({"sheet":"Data","range":"A2","color":{"theme":6,"tint":0.0}})).expect("theme target style is set");
    session
}

fn move_view(session: &mut Session, cell: &str) {
    session.execute("sheet.activate", json!({"sheet":"View"})).expect("view sheet activates");
    session.execute("selection.set", json!({"range":cell})).expect("view selection moves");
}

fn assert_entered_equal(a: &Session, b: &Session) {
    assert_eq!(fingerprint(a, DocId(0)).entered, fingerprint(b, DocId(0)).entered);
}

fn live_entry(payload: &str, stamp: Option<Stamp>) -> EntryView<'_> {
    EntryView { seq: None, participant: ParticipantNo(1), own: false, stamp, payload }
}

fn live_cell_set(action: u64, row: u32, input: &str, year: i32, formula: bool) -> String {
    json!({
        "version":1,
        "action":action,
        "command":{
            "id":"cell.set",
            "sheet":[0,0],
            "cell":{"sheet":[0,0],"row":[0,row],"col":[0,0]},
            "input":input,
            "originYear":year,
            "formula":formula,
            "references":[]
        }
    })
    .to_string()
}

fn live_server_frame(value: Json) -> String {
    value.to_string()
}

fn start_live_client<E: LiveEditor>(editor: &mut E) -> (LiveClient<E>, String) {
    let mut client = LiveClient::new(LiveConfig { format: GRIDCRAFT_LIVE_FORMAT.to_string(), crashed_at: None });
    let join = client.connected(0);
    let join: Json = serde_json::from_str(join.first().expect("join frame exists")).expect("join frame is JSON");
    let client_id = join["client"].as_str().expect("join names the client").to_string();
    client
        .receive(
            editor,
            &live_server_frame(json!({
                "type":"live_joined",
                "session":"00000000-0000-0000-0000-00000000005e",
                "participant":1,
                "place":{"is":"new"},
                "can_edit":true,
                "heartbeat_ms":60_000,
                "people":[]
            })),
            1,
        )
        .expect("join is accepted");
    let catchup = client
        .receive(editor, &live_server_frame(json!({"type":"catchup","base":0,"revision":null,"from":0,"head":0})), 2)
        .expect("empty catch-up is accepted");
    assert!(matches!(catchup.events.as_slice(), [Event::LoadBase { .. }]));
    client.base_ready(editor, 3).expect("empty base becomes live");
    (client, client_id)
}

struct LifecycleEditor {
    inner: GridLiveEditor,
    crash: bool,
    saw_applying: Option<Seq>,
}

impl LiveEditor for LifecycleEditor {
    type Snapshot = GridSnapshot;

    fn snapshot(&self) -> Self::Snapshot {
        self.inner.snapshot()
    }

    fn restore(&mut self, snapshot: &Self::Snapshot) {
        self.inner.restore(snapshot);
    }

    fn apply(&mut self, entry: &EntryView<'_>) -> Result<Outcome, Crash> {
        self.saw_applying = self.inner.applying_seq();
        if self.crash { Err(Crash) } else { self.inner.apply(entry) }
    }

    fn note_applying(&mut self, seq: Option<Seq>) {
        self.inner.note_applying(seq);
    }
}

fn cell_value(editor: &GridLiveEditor, row: u32) -> Value {
    editor.session().doc().expect("document exists").wb.sheet(0).expect("sheet exists").value(CellRef::new(row, 0))
}

#[test]
fn entry_stamp_controls_now_and_rand() {
    let mut left_session = session_with_sheets(&["Sheet1"]);
    let mut right_session = session_with_sheets(&["Sheet1"]);
    left_session.doc_mut().expect("left document exists").calc.fixed_now = Some(1.0);
    left_session.doc_mut().expect("left document exists").calc.set_rand_seed(11);
    right_session.doc_mut().expect("right document exists").calc.fixed_now = Some(2.0);
    right_session.doc_mut().expect("right document exists").calc.set_rand_seed(99);
    let mut left = GridLiveEditor::new(left_session).expect("left editor builds");
    let mut right = GridLiveEditor::new(right_session).expect("right editor builds");
    let payload = live_cell_set(1, 0, "=NOW()+RAND()", 2026, true);
    let stamp = Stamp { at_ms: 1_791_633_600_000, seed: 0x1234_5678 };

    let left_outcome = left.apply(&live_entry(&payload, Some(stamp))).expect("left applies the entry");
    let right_outcome = right.apply(&live_entry(&payload, Some(stamp))).expect("right applies the entry");

    assert!(!left_outcome.read_stamp);
    assert!(!right_outcome.read_stamp);
    let left_value = left.session().doc().expect("left document exists").wb.sheet(0).expect("left sheet exists").value(CellRef::new(0, 0));
    let right_value = right.session().doc().expect("right document exists").wb.sheet(0).expect("right sheet exists").value(CellRef::new(0, 0));
    assert_eq!(left_value, right_value);

    let unstamped = live_cell_set(2, 1, "=RAND()", 2026, true);
    assert!(left.apply(&live_entry(&unstamped, None)).expect("local volatile entry applies").read_stamp);
}

#[test]
fn formula_yearless_dates_read_the_stamped_clock() {
    let mut session = session_with_sheets(&["Sheet1"]);
    session.doc_mut().expect("document exists").calc.fixed_now = Some(gridcraft_core::date::serial_from_unix_ms(2_285_193_600_000.0));
    let mut editor = GridLiveEditor::new(session).expect("editor builds");

    for (action, row, input) in [(20, 0, "=DATEVALUE(\"12/31\")"), (21, 1, "=VALUE(\"12/31\")"), (22, 2, "=\"12/31\"+0"), (23, 3, "=ABS(\"12/31\")")]
    {
        let payload = live_cell_set(action, row, input, 2026, true);
        let outcome = editor.apply(&live_entry(&payload, None)).expect("formula applies");
        assert!(outcome.read_stamp, "{input} must report that it read the stamped clock");
        assert_eq!(cell_value(&editor, row), Value::Number(52_231.0), "{input} must use the stamped year");
    }

    let payload = live_cell_set(24, 4, "=YEAR(\"12/31\")", 2026, true);
    let outcome = editor.apply(&live_entry(&payload, None)).expect("YEAR applies");
    assert!(outcome.read_stamp, "YEAR must report that it read the stamped clock");
    assert_eq!(cell_value(&editor, 4), Value::Number(2042.0), "YEAR must use the stamped year");

    let payload = live_cell_set(25, 5, "=COUNTIF(52231,\"12/31\")", 2026, true);
    let outcome = editor.apply(&live_entry(&payload, None)).expect("COUNTIF applies");
    assert!(outcome.read_stamp, "criteria coercion must report that it read the stamped clock");
    assert_eq!(cell_value(&editor, 5), Value::Number(1.0), "criteria coercion must use the stamped year");

    let payload = live_cell_set(26, 6, "=INDEX({1;2},\"12/31\")", 2026, true);
    let outcome = editor.apply(&live_entry(&payload, None)).expect("INDEX applies");
    assert!(outcome.read_stamp, "special-form coercion must report that it read the stamped clock");
    assert_eq!(cell_value(&editor, 6), Value::Error(gridcraft_core::CellError::Ref));

    for (action, row, input, expected) in [
        (27, 7, "=NETWORKDAYS(52231,52231,\"12/31\")", Value::Number(0.0)),
        (28, 8, "=DSUM({\"Date\";52231},1,{\"Date\";\"12/31\"})", Value::Number(52_231.0)),
        (29, 9, "=XNPV(0.1,{-100;110},{52231;\"12/31\"})", Value::Number(10.0)),
    ] {
        let payload = live_cell_set(action, row, input, 2026, true);
        let outcome = editor.apply(&live_entry(&payload, None)).expect("formula applies");
        assert!(outcome.read_stamp, "{input} must report that it read the stamped clock");
        assert_eq!(cell_value(&editor, row), expected, "{input} must use the stamped year");
    }
}

#[test]
fn ordinary_text_formulas_do_not_read_the_stamp() {
    let mut editor = GridLiveEditor::new(session_with_sheets(&["Sheet1"])).expect("editor builds");

    for (action, row, input, expected) in [
        (40, 0, "=LEN(\"x\")", Value::Number(1.0)),
        (41, 1, "=LEFT(\"text\",2)", Value::text("te")),
        (42, 2, "=CONCAT(\"a\",\"b\")", Value::text("ab")),
        (45, 3, "=\"plain\"+0", Value::Error(gridcraft_core::CellError::Value)),
        (46, 4, "=INDEX({1;2},\"plain\")", Value::Error(gridcraft_core::CellError::Value)),
        (47, 5, "=YEAR(\"3/2025\")", Value::Number(2025.0)),
    ] {
        let payload = live_cell_set(action, row, input, 2026, true);
        let outcome = editor.apply(&live_entry(&payload, None)).expect("formula applies");
        assert!(!outcome.read_stamp, "{input} must not report a stamp read");
        assert_eq!(cell_value(&editor, row), expected);
    }
}

#[test]
fn large_text_range_does_not_read_the_stamp_before_coercion() {
    let mut session = session_with_sheets(&["Sheet1"]);
    {
        let document = session.doc_mut().expect("document exists");
        let workbook = Arc::make_mut(&mut document.wb);
        let sheet = workbook.sheet_mut(0).expect("sheet exists");
        for row in 0..100_000 {
            sheet.set_value(CellRef::new(row, 0), Value::text("x"));
        }
    }
    let mut editor = GridLiveEditor::new(session).expect("editor builds");
    let mut next = 0;
    let change = editor
        .prepare(43, "cell.set", &json!({"sheet":"Sheet1","cell":"B1","input":"=SUM(A1:A100000)"}), 2026, &mut |count| {
            let first = collab_live::ids::LiveId(7, next);
            next = next.checked_add(count).ok_or(collab_live::ids::IdError::NoPlace)?;
            Ok(first)
        })
        .expect("large-range formula prepares");

    let outcome = editor.apply(&live_entry(&change.payload, None)).expect("large-range formula applies");

    assert!(!outcome.read_stamp, "ordinary text in a large range must not read the stamp");
    let value = editor.session().doc().expect("document exists").wb.sheet(0).expect("sheet exists").value(CellRef::new(0, 1));
    assert_eq!(value, Value::Number(0.0));
}

#[test]
fn first_local_insert_follows_the_new_sheet_but_remote_view_does_not() {
    let mut sender = GridLiveEditor::new(session_with_sheets(&["Data", "View"])).expect("sender builds");
    move_view(sender.session_mut(), "C3");
    let mut remote = GridLiveEditor::new(session_with_sheets(&["Data", "View"])).expect("remote builds");
    move_view(remote.session_mut(), "D4");
    let (mut sender_client, sender_id) = start_live_client(&mut sender);
    let (mut remote_client, _) = start_live_client(&mut remote);
    let mut next = 0;
    let change = sender
        .prepare(44, "home.insertSheet", &json!({"name":"Local"}), 2026, &mut |count| {
            let first = collab_live::ids::LiveId(7, next);
            next = next.checked_add(count).ok_or(collab_live::ids::IdError::NoPlace)?;
            Ok(first)
        })
        .expect("insert prepares");

    sender_client.local(&mut sender, change.payload.clone(), change.mode, 4).expect("local insert is accepted");
    assert_eq!(sender.session().doc().expect("sender document exists").wb.active().expect("active sheet exists").name, "Local");

    remote_client
        .receive(
            &mut remote,
            &live_server_frame(json!({
                "type":"entry",
                "entry":{
                    "seq":1,
                    "at_ms":1_795_219_200_000i64,
                    "seed":41,
                    "kind":"change",
                    "participant":1,
                    "client":sender_id,
                    "local":0,
                    "seen":0,
                    "payload":change.payload,
                    "cancelled":false
                }
            })),
            5,
        )
        .expect("remote insert is accepted");
    let remote_document = remote.session().doc().expect("remote document exists");
    assert_eq!(remote_document.wb.active().expect("remote active sheet exists").name, "View");
    assert_eq!(remote_document.selection.active, CellRef::new(3, 3));
}

#[test]
fn live_client_reruns_yearless_date_with_the_server_stamp() {
    let mut session = session_with_sheets(&["Sheet1"]);
    session.doc_mut().expect("document exists").calc.fixed_now = Some(gridcraft_core::date::serial_from_unix_ms(2_285_193_600_000.0));
    let mut editor = GridLiveEditor::new(session).expect("editor builds");
    let (mut client, client_id) = start_live_client(&mut editor);
    let payload = live_cell_set(30, 0, "=DATEVALUE(\"12/31\")", 2026, true);

    let preview = client.local(&mut editor, payload.clone(), Mode::ShowAtOnce, 4).expect("local preview is accepted");
    assert_eq!(cell_value(&editor, 0), Value::Number(52_231.0), "the preview uses the local stamped year");
    let sent: Json = serde_json::from_str(preview.send.first().expect("preview is sent")).expect("entry frame is JSON");
    let confirmation = live_server_frame(json!({
        "type":"entry",
        "entry":{
            "seq":1,
            "at_ms":1_906_502_400_000i64,
            "seed":77,
            "kind":"change",
            "participant":1,
            "client":client_id,
            "local":sent["local"].clone(),
            "seen":sent["seen"].clone(),
            "payload":payload,
            "cancelled":false
        }
    }));

    let confirmed = client.receive(&mut editor, &confirmation, 5).expect("stamped confirmation is accepted");

    assert!(matches!(confirmed.events.as_slice(), [Event::Applied { own: true, .. }]));
    assert_eq!(client.pending(), 0);
    assert_eq!(editor.applying_seq(), None, "the successful confirmation clears its crash note");
    assert_eq!(cell_value(&editor, 0), Value::Number(47_848.0), "confirmation rolls back and uses the server year");
}

#[test]
fn live_client_keeps_the_applying_sequence_through_a_crash() {
    for crash in [false, true] {
        let inner = GridLiveEditor::new(session_with_sheets(&["Sheet1"])).expect("editor builds");
        let mut editor = LifecycleEditor { inner, crash, saw_applying: None };
        let (mut client, _) = start_live_client(&mut editor);
        let payload = live_cell_set(40, 0, "ok", 2026, false);
        let frame = live_server_frame(json!({
            "type":"entry",
            "entry":{
                "seq":1,
                "at_ms":1_906_502_400_000i64,
                "seed":88,
                "kind":"change",
                "participant":2,
                "client":"00000000-0000-0000-0000-000000000099",
                "local":1,
                "seen":0,
                "payload":payload,
                "cancelled":false
            }
        }));

        let step = client.receive(&mut editor, &frame, 4).expect("ordered entry is accepted");

        assert_eq!(editor.saw_applying.map(|seq| seq.0), Some(1), "the sequence is stored before apply");
        if crash {
            assert_eq!(editor.inner.applying_seq().map(|seq| seq.0), Some(1), "a crash preserves the sequence");
            assert!(matches!(step.events.as_slice(), [Event::Crashed { .. }]));
        } else {
            assert_eq!(editor.inner.applying_seq(), None, "success clears the sequence");
            assert!(matches!(step.events.as_slice(), [Event::Applied { .. }]));
        }
    }
}

#[test]
fn plain_edit_with_existing_volatile_formula_reads_stamp() {
    let mut session = session_with_sheets(&["Sheet1"]);
    session.execute("cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"=RAND()"})).expect("volatile formula applies");
    let mut editor = GridLiveEditor::new(session).expect("editor builds");
    let payload = live_cell_set(5, 1, "plain", 2026, false);

    let outcome = editor.apply(&live_entry(&payload, None)).expect("plain edit applies");

    assert!(matches!(outcome.effect, Effect::Applied));
    assert!(outcome.read_stamp, "recalculation read RAND from the existing formula");
}

#[test]
fn structural_edit_with_existing_volatile_formula_reads_stamp() {
    let mut session = session_with_sheets(&["Sheet1"]);
    session.execute("cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"=NOW()"})).expect("volatile formula applies");
    let mut editor = GridLiveEditor::new(session).expect("editor builds");
    let payload = json!({
        "version":1,
        "action":6,
        "command":{
            "id":"home.insertRows",
            "sheet":[0,0],
            "rows":{"before":{"before":[0,1]},"first":[1,0],"count":1}
        }
    })
    .to_string();

    let outcome = editor.apply(&live_entry(&payload, None)).expect("row insert applies");

    assert!(matches!(outcome.effect, Effect::Applied));
    assert!(outcome.read_stamp, "structural recalculation read NOW from the existing formula");
}

#[test]
fn yearless_date_uses_the_sent_year() {
    let mut session = session_with_sheets(&["Sheet1"]);
    let ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);
    let entry = encode_command(&ids, &session, 2, "cell.set", &json!({"sheet":"Sheet1","cell":"A1","input":"12/31"}), 2042, &mut allocate)
        .expect("year-less date encodes");
    let resolved = resolve_command(&ids, &session, &entry).expect("year-less date resolves");

    session.execute(&resolved.id, resolved.params).expect("year-less date applies");

    assert_eq!(session.doc().expect("document exists").wb.sheet(0).expect("sheet exists").value(CellRef::new(0, 0)), Value::Number(52_231.0));
    let before = fingerprint(&session, DocId(0)).entered;
    assert!(session.execute("cell.set", json!({"sheet":"Sheet1","cell":"A2","input":"12/31","originYear":10000})).is_err());
    assert_eq!(fingerprint(&session, DocId(0)).entered, before);
}

#[test]
fn snapshot_restores_rand_state() {
    let mut editor = GridLiveEditor::new(session_with_sheets(&["Sheet1"])).expect("editor builds");
    let seed_payload = live_cell_set(3, 0, "seed", 2026, false);
    editor.apply(&live_entry(&seed_payload, Some(Stamp { at_ms: 1_791_633_600_000, seed: 77 }))).expect("stamp seeds the calculation state");
    let snapshot = editor.snapshot();
    let rand_payload = live_cell_set(4, 1, "=RAND()", 2026, true);

    editor.apply(&live_entry(&rand_payload, None)).expect("first local RAND applies");
    let first = editor.session().doc().expect("document exists").wb.sheet(0).expect("sheet exists").value(CellRef::new(1, 0));
    editor.restore(&snapshot);
    editor.apply(&live_entry(&rand_payload, None)).expect("replayed local RAND applies");
    let replayed = editor.session().doc().expect("document exists").wb.sheet(0).expect("sheet exists").value(CellRef::new(1, 0));

    assert_eq!(first, replayed);
}

#[test]
fn agreement_digest_is_entered_only() {
    let mut left = session_with_sheets(&["Sheet1"]);
    left.execute("cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"=SIN(PI()/4)"})).expect("formula applies");
    let mut right = session_with_sheets(&["Sheet1"]);
    right.execute("cell.set", json!({"sheet":"Sheet1","cell":"A1","input":"=SIN(PI()/4)"})).expect("formula applies");
    let at = CellRef::new(0, 0);
    let right_doc = right.doc_mut().expect("right document exists");
    let workbook = Arc::make_mut(&mut right_doc.wb);
    let sheet = workbook.sheet_mut(0).expect("right sheet exists");
    let mut cell = sheet.cell(at).cloned().expect("formula cell exists");
    let Value::Number(number) = cell.value else { panic!("formula result is numeric") };
    cell.value = Value::Number(f64::from_bits(number.to_bits() + 1));
    sheet.set_cell(at, cell);
    assert_ne!(fingerprint(&left, DocId(0)).computed, fingerprint(&right, DocId(0)).computed);

    let left = GridLiveEditor::new(left).expect("left editor builds");
    let right = GridLiveEditor::new(right).expect("right editor builds");

    assert_eq!(left.agreement_digest(), right.agreement_digest());
}

#[test]
fn filter_commands_ignore_the_receivers_view() {
    let mut a = explicit_command_session();
    let mut b = explicit_command_session();
    a.execute("sheet.activate", json!({"sheet":"Data"})).expect("target sheet activates");
    a.execute("selection.set", json!({"range":"C4"})).expect("target selection moves");
    move_view(&mut b, "B1");

    let commands = [
        ("data.filterSet", json!({"sheet":"Data","range":"A1:C4"})),
        ("data.filterBy", json!({"sheet":"Data","range":"A1:C4","column":"B","values":["x"]})),
        ("data.reapply", json!({"sheet":"Data","range":"A1:C4"})),
        ("data.clearFilter", json!({"sheet":"Data","range":"A1:C4"})),
    ];
    for (id, params) in commands {
        let left = a.execute(id, params.clone()).unwrap_or_else(|error| panic!("{id} failed in the target view: {error}"));
        let right = b.execute(id, params).unwrap_or_else(|error| panic!("{id} failed in the unrelated view: {error}"));
        assert_eq!(left, right, "{id} returned a view-dependent result");
        assert_entered_equal(&a, &b);
    }

    for session in [&a, &b] {
        let document = session.doc().expect("document exists");
        let data = document.wb.sheet(document.wb.sheet_index("Data").expect("Data exists")).expect("Data exists");
        let view = document.wb.sheet(document.wb.sheet_index("View").expect("View exists")).expect("View exists");
        assert_eq!(data.autofilter.as_ref().map(|filter| filter.range.a1()), Some("A1:C4".into()));
        assert!(view.autofilter.is_none());
    }

    let params = json!({"sheet":"Data","range":"A1:C4"});
    assert_eq!(
        a.execute("data.filterRemove", params.clone()).expect("target filter is removed"),
        b.execute("data.filterRemove", params).expect("same filter is removed")
    );
    assert_entered_equal(&a, &b);
}

#[test]
fn filter_set_and_remove_are_idempotent() {
    let mut session = explicit_command_session();
    let target = json!({"sheet":"Data","range":"A1:C4"});

    session.execute("data.filterSet", target.clone()).expect("filter is set");
    let set = fingerprint(&session, DocId(0)).entered;
    session.execute("data.filterSet", target.clone()).expect("setting the same filter is harmless");
    assert_eq!(fingerprint(&session, DocId(0)).entered, set);

    let before_mismatch = fingerprint(&session, DocId(0)).entered;
    assert!(session.execute("data.filterSet", json!({"sheet":"Data","range":"A1:B4"})).is_err());
    assert_eq!(fingerprint(&session, DocId(0)).entered, before_mismatch, "a mismatched range must not replace the filter");

    session.execute("data.filterRemove", target.clone()).expect("filter is removed");
    let removed = fingerprint(&session, DocId(0)).entered;
    session.execute("data.filterRemove", target).expect("removing an absent filter is harmless");
    assert_eq!(fingerprint(&session, DocId(0)).entered, removed);
}

#[test]
fn repeated_filter_set_preserves_existing_criteria() {
    let mut session = explicit_command_session();
    let target = json!({"sheet":"Data","range":"A1:C4"});

    session.execute("data.filterSet", target.clone()).expect("filter is set");
    session.execute("data.filterBy", json!({"sheet":"Data","range":"A1:C4","column":"B","values":["x"]})).expect("criterion is set");
    let before = fingerprint(&session, DocId(0)).entered;

    session.execute("data.filterSet", target).expect("setting the same filter is harmless");

    assert_eq!(fingerprint(&session, DocId(0)).entered, before, "setting the same filter must preserve its criterion and filtered rows");
}

#[test]
fn filter_remove_refuses_a_mismatched_range_without_changes() {
    let mut session = explicit_command_session();
    session.execute("data.filterSet", json!({"sheet":"Data","range":"A1:C4"})).expect("filter is set");
    session.execute("data.filterBy", json!({"sheet":"Data","range":"A1:C4","column":"B","values":["x"]})).expect("criterion is set");
    let before = fingerprint(&session, DocId(0)).entered;

    assert!(session.execute("data.filterRemove", json!({"sheet":"Data","range":"A1:B4"})).is_err());
    assert_eq!(fingerprint(&session, DocId(0)).entered, before, "a mismatched remove must preserve the filter and its effects");
}

#[test]
fn underline_with_on_ignores_active_cell() {
    let mut a = explicit_command_session();
    let mut b = explicit_command_session();
    move_view(&mut a, "A1");
    move_view(&mut b, "B1");
    let params = json!({"sheet":"Data","range":"C2:C3","on":true,"style":"single"});

    assert_eq!(
        a.execute("home.underline", params.clone()).expect("explicit underline applies"),
        b.execute("home.underline", params).expect("explicit underline applies")
    );
    assert_entered_equal(&a, &b);
}

#[test]
fn double_underline_stores_the_supplied_accounting_style() {
    let mut session = explicit_command_session();

    session
        .execute("home.doubleUnderline", json!({"sheet":"Data","range":"C2:C3","on":true,"style":"doubleAccounting"}))
        .expect("explicit double accounting underline applies");

    assert_eq!(
        session.execute("cell.get", json!({"sheet":"Data","cell":"C2"})).expect("target reads")["style"]["font"]["underline"],
        "DoubleAccounting"
    );
}

#[test]
fn font_step_uses_the_sent_size() {
    let mut a = explicit_command_session();
    let mut b = explicit_command_session();
    move_view(&mut a, "A1");
    move_view(&mut b, "B1");
    let params = json!({"sheet":"Data","range":"C2:C3","size":14});

    assert_eq!(
        a.execute("home.increaseFontSize", params.clone()).expect("explicit size applies"),
        b.execute("home.increaseFontSize", params).expect("explicit size applies")
    );
    assert_entered_equal(&a, &b);
    for session in [&mut a, &mut b] {
        assert_eq!(session.execute("cell.get", json!({"sheet":"Data","cell":"C2"})).expect("target reads")["style"]["font"]["size"], 14.0);
    }
}

#[test]
fn decrease_font_step_uses_the_sent_size() {
    let mut a = explicit_command_session();
    let mut b = explicit_command_session();
    move_view(&mut a, "A1");
    move_view(&mut b, "B1");
    let params = json!({"sheet":"Data","range":"C2:C3","size":10});

    assert_eq!(
        a.execute("home.decreaseFontSize", params.clone()).expect("explicit decreased size applies"),
        b.execute("home.decreaseFontSize", params).expect("explicit decreased size applies")
    );
    assert_entered_equal(&a, &b);
    for session in [&mut a, &mut b] {
        assert_eq!(session.execute("cell.get", json!({"sheet":"Data","cell":"C2"})).expect("target reads")["style"]["font"]["size"], 10.0);
    }
}

#[test]
fn format_cells_validation_uses_each_target_not_active_cell() {
    let mut a = explicit_command_session();
    let mut b = explicit_command_session();
    move_view(&mut a, "A1");
    move_view(&mut b, "B1");
    let params = json!({"sheet":"Data","range":"A1:A2","style":{"font":{"color":{"Theme":[5,0]}}}});
    let before_a = fingerprint(&a, DocId(0)).entered;
    let before_b = fingerprint(&b, DocId(0)).entered;

    assert!(a.execute("home.formatCells", params.clone()).is_err(), "every target style must validate before editing");
    assert!(b.execute("home.formatCells", params).is_err(), "validation must not depend on the active style");
    assert_eq!(fingerprint(&a, DocId(0)).entered, before_a);
    assert_eq!(fingerprint(&b, DocId(0)).entered, before_b);
}

#[test]
fn base_workbook_identity_tables_are_single_runs() {
    let session = session_with_sheets(&["Sheet1", "Data"]);
    let workbook = &session.doc().expect("document exists").wb;

    let ids = WorkbookIds::from_workbook(workbook).expect("base identities build");

    assert_eq!(ids.sheets.len(), 2);
    assert_eq!(ids.sheets.runs(), 1);
    for sheet_index in 0..2u32 {
        let sheet_id = collab_live::ids::LiveId(0, sheet_index);
        let sheet = ids.by_id.get(&sheet_id).expect("base sheet identity exists");
        assert_eq!(sheet.id, sheet_id);
        assert_eq!(sheet.rows.runs(), 1);
        assert_eq!(sheet.cols.runs(), 1);
        assert_eq!(sheet.rows.id_at(0), Some(collab_live::ids::LiveId(0, 0)));
        assert_eq!(sheet.cols.id_at(0), Some(collab_live::ids::LiveId(0, 0)));
    }
}

#[test]
fn concurrent_row_inserts_keep_both_identity_runs() {
    let session = session_with_sheets(&["Sheet1"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let sheet = collab_live::ids::LiveId(0, 0);
    let before = IdentityPosition::Before(collab_live::ids::LiveId(0, 4));

    ids.apply_structural(&IdentityStructural::InsertRows { sheet, before, first: collab_live::ids::LiveId(1, 0), count: 2 })
        .expect("first insert applies");
    ids.apply_structural(&IdentityStructural::InsertRows { sheet, before, first: collab_live::ids::LiveId(2, 0), count: 2 })
        .expect("second insert applies");

    let rows = &ids.by_id.get(&sheet).expect("sheet identities exist").rows;
    assert_eq!(rows.id_at(4), Some(collab_live::ids::LiveId(1, 0)));
    assert_eq!(rows.id_at(5), Some(collab_live::ids::LiveId(1, 1)));
    assert_eq!(rows.id_at(6), Some(collab_live::ids::LiveId(2, 0)));
    assert_eq!(rows.id_at(7), Some(collab_live::ids::LiveId(2, 1)));
    assert_eq!(rows.id_at(8), Some(collab_live::ids::LiveId(0, 4)));
    assert_eq!(rows.len(), gridcraft_core::MAX_ROWS);
}

#[test]
fn structural_axes_stay_fixed_and_reject_invalid_counts() {
    let session = session_with_sheets(&["Sheet1"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let sheet = collab_live::ids::LiveId(0, 0);

    assert!(
        ids.apply_structural(&IdentityStructural::InsertRows {
            sheet,
            before: IdentityPosition::Before(collab_live::ids::LiveId(0, 0)),
            first: collab_live::ids::LiveId(1, 0),
            count: 0,
        })
        .is_err()
    );
    ids.apply_structural(&IdentityStructural::InsertColumns {
        sheet,
        before: IdentityPosition::Before(collab_live::ids::LiveId(0, gridcraft_core::MAX_COLS - 1)),
        first: collab_live::ids::LiveId(1, 1),
        count: 1,
    })
    .expect("last-column insert applies");
    assert_eq!(ids.by_id.get(&sheet).expect("sheet exists").cols.len(), gridcraft_core::MAX_COLS);
    assert!(matches!(
        ids.by_id.get(&sheet).expect("sheet exists").cols.find(collab_live::ids::LiveId(0, gridcraft_core::MAX_COLS - 1)),
        collab_live::ids::Lookup::Deleted { .. }
    ));
}

#[test]
fn deleted_target_resolves_to_no_effect() {
    let session = session_with_sheets(&["Sheet1"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let target = ids.cell_id(0, gridcraft_core::CellRef::new(3, 2)).expect("cell identity resolves");

    ids.apply_structural(&IdentityStructural::DeleteRows { sheet: target.sheet, ids: vec![target.row], first: collab_live::ids::LiveId(9, 0) })
        .expect("row delete applies");

    let error = ids.resolve_cell(target).expect_err("deleted target must not move to another cell");
    assert_eq!(error.to_string(), "row identity LiveId(0, 3) was deleted");
}

#[test]
fn formula_references_round_trip_through_identities() {
    let session = session_with_sheets(&["Sheet1", "Data", "Tail"]);
    let ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let input = "=A1+$B$2+Data!C3+SUM(D4:E5,6:7,F:G,Sheet1:Tail!$H8)";
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);

    let entry = encode_command(&ids, &session, 7, "cell.set", &json!({"sheet":"Sheet1","cell":"J10","input":input}), 2026, &mut allocate)
        .expect("formula command encodes");
    let resolved = resolve_command(&ids, &session, &entry).expect("formula command resolves");
    let resolved_input = resolved.params["input"].as_str().expect("resolved input is text");

    assert_eq!(
        parse(resolved_input.trim_start_matches('=')).expect("resolved formula parses"),
        parse(input.trim_start_matches('=')).expect("source formula parses")
    );
    assert_eq!(
        print(&parse(resolved_input.trim_start_matches('=')).expect("resolved formula parses")),
        print(&parse(input.trim_start_matches('=')).expect("source formula parses"))
    );
}

#[test]
fn formula_deleted_reference_becomes_ref() {
    let session = session_with_sheets(&["Sheet1"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);
    let entry = encode_command(&ids, &session, 8, "cell.set", &json!({"sheet":"Sheet1","cell":"C3","input":"=A1+B2"}), 2026, &mut allocate)
        .expect("formula command encodes");
    ids.apply_structural(&IdentityStructural::DeleteRows {
        sheet: collab_live::ids::LiveId(0, 0),
        ids: vec![collab_live::ids::LiveId(0, 0)],
        first: collab_live::ids::LiveId(9, 1),
    })
    .expect("row delete applies");

    let resolved = resolve_command(&ids, &session, &entry).expect("entry itself remains well formed");

    assert_eq!(resolved.params["input"], "=#REF!+B1");
}

#[test]
fn signed_formulas_use_identities_and_text_formatted_equals_stays_text() {
    let session = session_with_sheets(&["Sheet1"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);
    let entry = encode_command(&ids, &session, 12, "cell.set", &json!({"sheet":"Sheet1","cell":"C3","input":"+A1"}), 2026, &mut allocate)
        .expect("signed formula encodes");
    ids.apply_structural(&IdentityStructural::DeleteRows {
        sheet: collab_live::ids::LiveId(0, 0),
        ids: vec![collab_live::ids::LiveId(0, 0)],
        first: collab_live::ids::LiveId(9, 2),
    })
    .expect("row delete applies");
    let resolved = resolve_command(&ids, &session, &entry).expect("signed formula resolves");
    assert_eq!(resolved.params["input"], "=+#REF!");

    let mut text_session = session_with_sheets(&["Sheet1"]);
    text_session.execute("home.numberFormat", json!({"sheet":"Sheet1","range":"C3","code":"@"})).expect("text format applies");
    let text_ids = WorkbookIds::from_workbook(&text_session.doc().expect("document exists").wb).expect("base identities build");
    let text_entry =
        encode_command(&text_ids, &text_session, 13, "cell.set", &json!({"sheet":"Sheet1","cell":"C3","input":"=A1"}), 2026, &mut allocate)
            .expect("text input encodes");
    match text_entry.command {
        SharedCommand::CellSet { input, formula, references, .. } => {
            assert_eq!(input, "=A1");
            assert!(!formula);
            assert!(references.is_empty());
        }
        other => panic!("expected cell.set, got {other:?}"),
    }
}

#[test]
fn cell_set_formula_marker_is_required_and_consistent() {
    let session = session_with_sheets(&["Sheet1"]);
    let ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let missing_marker = json!({
        "version":1,
        "action":20,
        "command":{
            "id":"cell.set",
            "sheet":[0,0],
            "cell":cell_identity(0,0),
            "input":"=A1",
            "originYear":2026,
            "references":[]
        }
    });
    assert!(matches!(
        decode_entry(&missing_marker.to_string()),
        Err(CoauthorError::MissingTarget { command, target }) if command == "cell.set" && target == "formula"
    ));

    let positional_formula = decode_entry(&entry(json!({
        "id":"cell.set",
        "sheet":[0,0],
        "cell":cell_identity(0,0),
        "input":"=A1",
        "originYear":2026,
        "formula":false,
        "references":[]
    })))
    .expect("typed literal entry decodes");
    assert!(matches!(resolve_command(&ids, &session, &positional_formula), Err(TargetGone::Malformed(_))));

    let false_with_references = decode_entry(&entry(json!({
        "id":"cell.set",
        "sheet":[0,0],
        "cell":cell_identity(0,0),
        "input":"plain text",
        "originYear":2026,
        "formula":false,
        "references":[{
            "sheet":{"kind":"current","sheet":[0,0]},
            "reference":{"kind":"cell","anchor":{"row":[0,0],"col":[0,0],"row_abs":false,"col_abs":false}}
        }]
    })))
    .expect("typed literal with references decodes");
    assert!(matches!(resolve_command(&ids, &session, &false_with_references), Err(TargetGone::Malformed(_))));

    let true_without_formula = decode_entry(&entry(json!({
        "id":"cell.set",
        "sheet":[0,0],
        "cell":cell_identity(0,0),
        "input":"A1",
        "originYear":2026,
        "formula":true,
        "references":[]
    })))
    .expect("typed formula entry decodes");
    assert!(matches!(resolve_command(&ids, &session, &true_without_formula), Err(TargetGone::Malformed(_))));

    let mut text_session = session_with_sheets(&["Sheet1"]);
    text_session.execute("home.numberFormat", json!({"sheet":"Sheet1","range":"A1","code":"@"})).expect("text format applies");
    let text_ids = WorkbookIds::from_workbook(&text_session.doc().expect("document exists").wb).expect("base identities build");
    let text_literal = decode_entry(&entry(json!({
        "id":"cell.set",
        "sheet":[0,0],
        "cell":cell_identity(0,0),
        "input":"=A1",
        "originYear":2026,
        "formula":false,
        "references":[]
    })))
    .expect("typed text literal decodes");
    assert_eq!(resolve_command(&text_ids, &text_session, &text_literal).expect("text literal resolves").params["input"], "=A1");
}

#[test]
fn multi_area_and_delete_count_targets_survive_encoding() {
    let session = session_with_sheets(&["Sheet1"]);
    let ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let mut next = 0u32;
    let mut allocate = |count| {
        let first = collab_live::ids::LiveId(7, next);
        next = next.checked_add(count).ok_or(collab_live::ids::IdError::Exhausted)?;
        Ok(first)
    };
    let clear = encode_command(&ids, &session, 14, "edit.clearContents", &json!({"sheet":"Sheet1","range":"A1,C1"}), 2026, &mut allocate)
        .expect("multi-area clear encodes");
    assert_eq!(resolve_command(&ids, &session, &clear).expect("multi-area clear resolves").params["range"], "A1,C1");

    let delete = encode_command(&ids, &session, 15, "home.deleteRows", &json!({"sheet":"Sheet1","rows":"3","count":2}), 2026, &mut allocate)
        .expect("counted delete encodes");
    assert_eq!(resolve_command(&ids, &session, &delete).expect("counted delete resolves").params["rows"], "3:3,4:4");
}

#[test]
fn whole_axis_ranges_ignore_orthogonal_deleted_identities() {
    let session = session_with_sheets(&["Sheet1"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let row_range = ids.range_id(0, gridcraft_core::RangeRef::rows(4, 4)).expect("row identity range builds");
    ids.apply_structural(&IdentityStructural::DeleteColumns {
        sheet: collab_live::ids::LiveId(0, 0),
        ids: vec![collab_live::ids::LiveId(0, 0)],
        first: collab_live::ids::LiveId(9, 3),
    })
    .expect("column delete applies");

    assert_eq!(ids.resolve_range(row_range).expect("row target survives orthogonal delete").1.a1(), "5:5");
}

#[test]
fn structurally_divergent_three_d_reference_becomes_ref() {
    let session = session_with_sheets(&["Sheet1", "Middle", "Tail"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);
    let entry =
        encode_command(&ids, &session, 16, "cell.set", &json!({"sheet":"Sheet1","cell":"C3","input":"=SUM(Sheet1:Tail!A1)"}), 2026, &mut allocate)
            .expect("3-D formula encodes");
    ids.apply_structural(&IdentityStructural::InsertRows {
        sheet: collab_live::ids::LiveId(0, 0),
        before: IdentityPosition::Before(collab_live::ids::LiveId(0, 0)),
        first: collab_live::ids::LiveId(8, 0),
        count: 1,
    })
    .expect("one sheet shifts");

    let resolved = resolve_command(&ids, &session, &entry).expect("3-D entry resolves safely");
    assert_eq!(resolved.params["input"], "=SUM(#REF!)");
}

#[test]
fn three_d_reference_tolerates_interior_sheet_insertion() {
    let mut session = session_with_sheets(&["Sheet1", "Middle", "Tail"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);
    let entry =
        encode_command(&ids, &session, 21, "cell.set", &json!({"sheet":"Sheet1","cell":"C3","input":"=SUM(Sheet1:Tail!A1)"}), 2026, &mut allocate)
            .expect("3-D formula encodes");

    session.execute("home.insertSheet", json!({"name":"Inserted","before":1})).expect("interior sheet inserts");
    ids.apply_structural(&IdentityStructural::InsertSheet {
        before: IdentityPosition::Before(collab_live::ids::LiveId(0, 1)),
        id: collab_live::ids::LiveId(8, 0),
    })
    .expect("sheet identity inserts");

    let resolved = resolve_command(&ids, &session, &entry).expect("3-D entry survives an interior insertion");
    assert_eq!(resolved.params["input"], "=SUM(Sheet1:Tail!A1)");
}

#[test]
fn three_d_reference_tolerates_interior_sheet_deletion() {
    let mut session = session_with_sheets(&["Sheet1", "Middle", "Tail"]);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);
    let entry =
        encode_command(&ids, &session, 22, "cell.set", &json!({"sheet":"Sheet1","cell":"C3","input":"=SUM(Sheet1:Tail!A1)"}), 2026, &mut allocate)
            .expect("3-D formula encodes");

    session.execute("home.deleteSheet", json!({"sheet":"Middle"})).expect("interior sheet deletes");
    ids.apply_structural(&IdentityStructural::DeleteSheet { id: collab_live::ids::LiveId(0, 1) }).expect("sheet identity deletes");

    let resolved = resolve_command(&ids, &session, &entry).expect("3-D entry survives an interior deletion");
    assert_eq!(resolved.params["input"], "=SUM(Sheet1:Tail!A1)");
}

#[test]
fn structural_resolution_rejects_reused_identity_runs_atomically() {
    let session = session_with_sheets(&["Sheet1"]);
    let ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let ids_before = ids.clone();
    let workbook_before = session.doc().expect("document exists").wb.clone();
    let overlapping_insert = decode_entry(&entry(json!({
        "id":"home.insertRows",
        "sheet":[0,0],
        "rows":{"before":{"before":[0,0]},"first":[0,1048575],"count":2}
    })))
    .expect("typed insert decodes");
    let reused_replacement = decode_entry(&entry(json!({
        "id":"home.deleteRows",
        "sheet":[0,0],
        "rows":{"ids":[[0,0]],"first":[0,1]}
    })))
    .expect("typed delete decodes");

    assert!(matches!(resolve_command(&ids, &session, &overlapping_insert), Err(TargetGone::Malformed(_))));
    assert!(matches!(resolve_command(&ids, &session, &reused_replacement), Err(TargetGone::Malformed(_))));
    assert_eq!(ids, ids_before);
    assert_eq!(session.doc().expect("document exists").wb, workbook_before);
}

#[test]
fn three_d_encoding_rejects_huge_span_before_walking_members() {
    let names: Vec<String> = (0..5_000).map(|index| format!("S{index}")).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let session = session_with_sheets(&refs);
    let mut ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    ids.by_id.remove(&collab_live::ids::LiveId(0, 1));
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);

    assert!(matches!(
        encode_command(&ids, &session, 23, "cell.set", &json!({"sheet":"S0","cell":"C3","input":"=SUM(S0:S4999!A1)"}), 2026, &mut allocate,),
        Err(CoauthorError::OversizedPayload { .. })
    ));
}

#[test]
fn two_clients_apply_structural_entries_to_identical_identity_tables() {
    let session = session_with_sheets(&["Sheet1", "Data"]);
    let mut first = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("first identities build");
    let mut second = first.clone();
    let entries = [
        IdentityStructural::InsertRows {
            sheet: collab_live::ids::LiveId(0, 0),
            before: IdentityPosition::Before(collab_live::ids::LiveId(0, 2)),
            first: collab_live::ids::LiveId(1, 0),
            count: 2,
        },
        IdentityStructural::DeleteRows {
            sheet: collab_live::ids::LiveId(0, 0),
            ids: vec![collab_live::ids::LiveId(0, 1)],
            first: collab_live::ids::LiveId(1, 2),
        },
        IdentityStructural::InsertColumns {
            sheet: collab_live::ids::LiveId(0, 0),
            before: IdentityPosition::Before(collab_live::ids::LiveId(0, 3)),
            first: collab_live::ids::LiveId(2, 0),
            count: 1,
        },
        IdentityStructural::DeleteColumns {
            sheet: collab_live::ids::LiveId(0, 0),
            ids: vec![collab_live::ids::LiveId(0, 1)],
            first: collab_live::ids::LiveId(2, 1),
        },
        IdentityStructural::InsertSheet { before: IdentityPosition::Before(collab_live::ids::LiveId(0, 1)), id: collab_live::ids::LiveId(3, 0) },
        IdentityStructural::DeleteSheet { id: collab_live::ids::LiveId(0, 1) },
    ];

    for change in entries {
        first.apply_structural(&change).expect("first client applies log entry");
        second.apply_structural(&change).expect("second client applies log entry");
        assert_eq!(first, second);
    }
}

#[test]
fn identity_payloads_are_closed_and_local_encoding_is_bounded() {
    let extra_cell = json!({
        "version":1,
        "action":17,
        "command":{
            "id":"cell.set",
            "sheet":[0,0],
            "cell":{"sheet":[0,0],"row":[0,0],"col":[0,0],"extra":false},
            "input":"1",
            "originYear":2026,
            "formula":false,
            "references":[]
        }
    });
    assert!(matches!(decode_entry(&extra_cell.to_string()), Err(CoauthorError::MalformedPayload(_))));

    let extra_range = json!({
        "version":1,
        "action":17,
        "command":{
            "id":"home.bold",
            "sheet":[0,0],
            "range":{
                "sheet":[0,0],
                "start":cell_identity(0,0),
                "end":cell_identity(0,0),
                "kind":"cells",
                "extra":false
            },
            "on":true
        }
    });
    assert!(matches!(decode_entry(&extra_range.to_string()), Err(CoauthorError::MalformedPayload(_))));

    let extra_reference = json!({
        "version":1,
        "action":17,
        "command":{
            "id":"cell.set",
            "sheet":[0,0],
            "cell":cell_identity(0,0),
            "input":"=A1",
            "originYear":2026,
            "formula":true,
            "references":[{
                "sheet":{"kind":"current","sheet":[0,0],"extra":false},
                "reference":{"kind":"cell","anchor":{"row":[0,0],"col":[0,0],"row_abs":false,"col_abs":false}}
            }]
        }
    });
    assert!(matches!(decode_entry(&extra_reference.to_string()), Err(CoauthorError::MalformedPayload(_))));

    let session = session_with_sheets(&["Sheet1"]);
    let ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let mut allocate = |_count| Err(collab_live::ids::IdError::NoPlace);
    assert!(matches!(
        encode_command(&ids, &session, 18, "home.deleteRows", &json!({"sheet":"Sheet1","rows":"1:50000"}), 2026, &mut allocate,),
        Err(CoauthorError::OversizedPayload { .. })
    ));
}

#[test]
fn mismatched_identity_targets_are_refused() {
    let session = session_with_sheets(&["Sheet1", "Data"]);
    let ids = WorkbookIds::from_workbook(&session.doc().expect("document exists").wb).expect("base identities build");
    let payload = json!({
        "version":1,
        "action":9,
        "command":{
            "id":"home.bold",
            "sheet":[0,0],
            "range":{
                "sheet":[0,1],
                "start":{"sheet":[0,1],"row":[0,0],"col":[0,0]},
                "end":{"sheet":[0,1],"row":[0,0],"col":[0,0]},
                "kind":"cells"
            },
            "on":true
        }
    });
    let entry = decode_entry(&payload.to_string()).expect("typed entry decodes");

    assert!(matches!(resolve_command(&ids, &session, &entry), Err(TargetGone::Malformed(_))));

    let mismatched_cell = decode_entry(
        &json!({
            "version":1,
            "action":10,
            "command":{
                "id":"cell.set",
                "sheet":[0,0],
                "cell":{"sheet":[0,1],"row":[0,0],"col":[0,0]},
                "input":"1",
                "originYear":2026,
                "formula":false,
                "references":[]
            }
        })
        .to_string(),
    )
    .expect("typed cell entry decodes");
    assert!(matches!(resolve_command(&ids, &session, &mismatched_cell), Err(TargetGone::Malformed(_))));

    let mismatched_current_reference = decode_entry(
        &json!({
            "version":1,
            "action":11,
            "command":{
                "id":"cell.set",
                "sheet":[0,1],
                "cell":{"sheet":[0,1],"row":[0,0],"col":[0,0]},
                "input":"=A1",
                "originYear":2026,
                "formula":true,
                "references":[{
                    "sheet":{"kind":"current","sheet":[0,0]},
                    "reference":{"kind":"cell","anchor":{"row":[0,0],"col":[0,0],"row_abs":false,"col_abs":false}}
                }]
            }
        })
        .to_string(),
    )
    .expect("typed formula entry decodes");
    assert!(matches!(resolve_command(&ids, &session, &mismatched_current_reference), Err(TargetGone::Malformed(_))));
}

fn canonical_commands() -> Vec<Json> {
    vec![
        json!({"id":"cell.set","sheet":[0,0],"cell":cell_identity(0,0),"input":"1","originYear":2026,"formula":false,"references":[]}),
        json!({"id":"edit.clearContents","sheet":[0,0],"range":range_identity((0,0),(1,1),"cells")}),
        json!({"id":"home.numberFormat","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"code":"0.00"}),
        json!({"id":"home.fontName","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"name":"Arial"}),
        json!({"id":"home.fontSize","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"size":12.0}),
        json!({"id":"home.fontColor","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"color":{"theme":4,"tint":-0.25}}),
        json!({"id":"home.fillColor","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"color":"#FFFF00"}),
        json!({"id":"home.borders","sheet":[0,0],"range":range_identity((0,0),(1,1),"cells"),"preset":"outside","style":"thin","color":"auto"}),
        json!({"id":"home.alignTop","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells")}),
        json!({"id":"home.alignMiddle","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells")}),
        json!({"id":"home.alignBottom","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells")}),
        json!({"id":"home.bold","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true}),
        json!({"id":"home.italic","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true}),
        json!({"id":"home.wrapText","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true}),
        json!({"id":"home.alignLeft","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true}),
        json!({"id":"home.alignCenter","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true}),
        json!({"id":"home.alignRight","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true}),
        json!({"id":"home.underline","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true,"style":"single"}),
        json!({"id":"home.doubleUnderline","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"on":true,"style":"double"}),
        json!({"id":"home.increaseFontSize","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"size":14.0}),
        json!({"id":"home.decreaseFontSize","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"size":10.0}),
        json!({"id":"home.formatCells","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"style":{"font":{"bold":true},"align":{"readingOrder":null}}}),
        json!({"id":"data.filterSet","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells")}),
        json!({"id":"data.filterRemove","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells")}),
        json!({"id":"data.filterBy","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells"),"column":[0,1],"values":["East"],"blanks":false}),
        json!({"id":"data.clearFilter","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells")}),
        json!({"id":"data.reapply","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells")}),
        json!({"id":"home.insertRows","sheet":[0,0],"rows":{"before":{"before":[0,2]},"first":[7,0],"count":2}}),
        json!({"id":"home.deleteRows","sheet":[0,0],"rows":{"ids":[[0,2],[0,3]],"first":[7,4]}}),
        json!({"id":"home.insertColumns","sheet":[0,0],"cols":{"before":{"before":[0,2]},"first":[7,2],"count":1}}),
        json!({"id":"home.deleteColumns","sheet":[0,0],"cols":{"ids":[[0,2]],"first":[7,6]}}),
        json!({"id":"home.insertSheet","name":"Data","before":{"before":{"before":[0,0]},"id":[7,3]}}),
        json!({"id":"sheet.rename","sheet":[0,0],"name":"Inputs"}),
        json!({"id":"home.deleteSheet","sheet":[0,0]}),
    ]
}

#[test]
fn shared_entry_round_trips_with_format_one() {
    let payload = r#"{"version":1,"action":42,"command":{"id":"home.bold","sheet":[0,0],"range":{"sheet":[0,0],"start":{"sheet":[0,0],"row":[0,0],"col":[0,0]},"end":{"sheet":[0,0],"row":[0,1],"col":[0,1]},"kind":"cells"},"on":true}}"#;

    let entry = decode_entry(payload).expect("gridcraft/1 entry decodes");

    assert_eq!(GRIDCRAFT_LIVE_FORMAT, "gridcraft/1");
    assert_eq!(entry.version, 1);
    assert_eq!(entry.action, 42);
    assert_eq!(serde_json::to_string(&entry).expect("entry encodes"), payload);
}

#[test]
fn shared_entry_rejects_unknown_keys_and_versions() {
    let extra = r#"{"version":1,"action":1,"command":{"id":"home.bold","sheet":[0,0],"range":"A1","on":true},"extra":false}"#;
    let version = r#"{"version":2,"action":1,"command":{"id":"home.bold","sheet":[0,0],"range":"A1","on":true}}"#;
    let variant = r#"{"version":1,"action":1,"command":{"id":"home.sparkle","sheet":[0,0],"range":"A1"}}"#;

    assert!(matches!(decode_entry(extra), Err(CoauthorError::MalformedPayload(_))));
    assert_eq!(decode_entry(version).expect_err("version 2 is unsupported").to_string(), "unsupported GridCraft live entry version 2");
    assert_eq!(decode_entry(variant).expect_err("unknown command is unsupported").to_string(), "command `home.sparkle` is not shared");

    let oversized = " ".repeat(262_145);
    assert!(matches!(decode_entry(&oversized), Err(CoauthorError::OversizedPayload { bytes: 262_145, max: 262_144 })));
}

#[test]
fn unlisted_commands_are_refused_in_live_mode() {
    for id in ["data.filter", "edit.undo", "macro.run", "automate.runMacro"] {
        assert!(!is_shared_command(id), "{id} must stay out of the live log");
    }
    for id in ["cell.set", "edit.clearContents", "home.bold", "data.filterSet", "home.insertRows", "sheet.rename"] {
        assert!(is_shared_command(id), "{id} is in the G3 shared surface");
    }
}

#[test]
fn shared_commands_require_explicit_targets() {
    let cases = [
        (r#"{"version":1,"action":1,"command":{"id":"home.bold","range":"A1","on":true}}"#, "command `home.bold` is missing required `sheet`"),
        (
            r#"{"version":1,"action":1,"command":{"id":"edit.clearContents","sheet":[0,0]}}"#,
            "command `edit.clearContents` is missing required `range`",
        ),
        (r#"{"version":1,"action":1,"command":{"id":"home.bold","sheet":[0,0],"range":"A1"}}"#, "command `home.bold` is missing required `on`"),
        (
            r#"{"version":1,"action":1,"command":{"id":"home.increaseFontSize","sheet":[0,0],"range":"A1"}}"#,
            "command `home.increaseFontSize` is missing required `size`",
        ),
    ];

    for (payload, expected) in cases {
        assert_eq!(decode_entry(payload).expect_err("required target is absent").to_string(), expected);
    }
}

#[test]
fn every_planned_command_accepts_its_canonical_payload_and_other_catalog_commands_are_refused() {
    let commands = canonical_commands();
    assert_eq!(commands.len(), 34);

    for command in &commands {
        let id = command["id"].as_str().expect("canonical command id is a string");
        let decoded = decode_entry(&entry(command.clone())).unwrap_or_else(|error| panic!("canonical `{id}` was refused: {error}"));
        assert_eq!(serde_json::to_value(decoded).expect("canonical entry re-encodes")["command"], *command);
        assert!(is_shared_command(id), "canonical `{id}` is absent from the allow-list");
    }

    for (id, _, _, _) in CATALOG {
        let planned = commands.iter().any(|command| command["id"] == *id);
        assert_eq!(is_shared_command(id), planned, "catalog command `{id}` has the wrong live-mode status");
    }
}

#[test]
fn shared_entry_rejects_extra_and_duplicate_keys_at_every_wire_level() {
    let cases = [
        r#"{"version":1,"action":1,"command":{"id":"home.bold","sheet":[0,0],"range":"A1","on":true},"extra":false}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.bold","sheet":[0,0],"range":"A1","on":true,"extra":false}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"font":{"bold":true},"extra":false}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"font":{"bold":true,"extra":false}}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"fill":{"pattern":"Solid","extra":false}}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"borders":{"top":{"style":"Thin","extra":false}}}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"align":{"wrap":true,"extra":false}}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"protection":{"locked":true,"extra":false}}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.fontColor","sheet":[0,0],"range":"A1","color":{"theme":4,"extra":false}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"data.filterBy","sheet":[0,0],"range":"A1:B2","column":[0,0],"custom":{"op":"=","value":"x","extra":false}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"data.filterBy","sheet":[0,0],"range":"A1:B2","column":[0,0],"top":{"count":10,"extra":false}}}"#,
        r#"{"version":1,"version":1,"action":1,"command":{"id":"home.bold","sheet":[0,0],"range":"A1","on":true}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.bold","id":"home.bold","sheet":[0,0],"range":"A1","on":true}}"#,
        r#"{"version":1,"action":1,"command":{"id":"home.formatCells","sheet":[0,0],"range":"A1","style":{"font":{"bold":true,"bold":false}}}}"#,
        r#"{"version":1,"action":1,"command":{"id":"data.filterBy","sheet":[0,0],"range":"A1:B2","column":[0,0],"top":{"count":10,"count":5}}}"#,
    ];

    for payload in cases {
        assert!(matches!(decode_entry(payload), Err(CoauthorError::MalformedPayload(_))), "accepted non-closed payload: {payload}");
    }
}

#[test]
fn shared_commands_accept_engine_defaulted_nested_fields_and_arbitrary_filter_values() {
    let commands = [
        json!({"id":"home.formatCells","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"style":{"borders":{"top":{"style":"Thin"}}}}),
        json!({"id":"home.formatCells","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"style":{"borders":{"top":{"color":"Auto"}}}}),
        json!({"id":"home.fontColor","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"color":{"theme":4}}),
        json!({"id":"data.filterBy","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells"),"column":[0,1],"values":[null,{"region":"East"},[1,2]]}),
        json!({"id":"data.filterBy","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells"),"column":[0,1],"custom":{}}),
    ];

    for command in commands {
        decode_entry(&entry(command.clone())).unwrap_or_else(|error| panic!("engine-compatible command was refused: {command}: {error}"));
    }
}

#[test]
fn filter_by_accepts_each_direct_closed_criteria_shape() {
    let criteria = [
        json!({"values":["East",2,true],"blanks":true}),
        json!({"custom":{"op":">=","value":2,"op2":"<","value2":40,"and":true}}),
        json!({"top":{"count":10,"bottom":false,"percent":true}}),
        json!({"aboveAverage":true}),
        json!({"clear":true}),
    ];

    for criterion in criteria {
        let mut command = json!({"id":"data.filterBy","sheet":[0,0],"range":range_identity((0,0),(8,2),"cells"),"column":[0,1]});
        command.as_object_mut().expect("command is an object").extend(criterion.as_object().expect("criterion is an object").clone());
        decode_entry(&entry(command)).expect("direct filter criteria decodes");
    }
}

#[test]
fn payload_limit_accepts_exactly_max_bytes_and_rejects_one_more() {
    let base = entry(json!({"id":"home.numberFormat","sheet":[0,0],"range":range_identity((0,0),(0,0),"cells"),"code":"0.00"}));
    let exact = format!("{base}{}", " ".repeat(MAX_PAYLOAD_BYTES - base.len()));
    let over = format!("{exact} ");

    decode_entry(&exact).expect("a payload exactly at the limit decodes");
    assert!(
        matches!(decode_entry(&over), Err(CoauthorError::OversizedPayload { bytes, max }) if bytes == MAX_PAYLOAD_BYTES + 1 && max == MAX_PAYLOAD_BYTES)
    );
}
