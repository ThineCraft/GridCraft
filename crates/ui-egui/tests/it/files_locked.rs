//! A host that owns the open file can lock it: the person can't swap it for another workbook
//! (New, Open, Close, the file picker, drops), while the host's own open (`host_open`, which
//! replaces the workbook shown) and the person's Save keep working.

use egui::os::OperatingSystem;
use egui::{Event, Key, Modifiers};
use egui_kittest::kittest::Queryable as _;
use gridcraft_engine::{Session, core::CellRef, core::Value};
use gridcraft_ui_egui::{Inbox, Services, SheetApp};
use serde_json::json;

type Harness = egui_kittest::Harness<'static, SheetApp>;

/// Ctrl on Windows and Linux sets both `ctrl` and `command`.
const CTRL: Modifiers = Modifiers { alt: false, ctrl: true, shift: false, mac_cmd: false, command: true };

fn harness(locked: bool, services: Services) -> Harness {
    let mut session = Session::new();
    session.new_workbook();
    let mut app = crate::common::prepare(SheetApp::new(session, services));
    app.files_locked = locked;
    crate::common::harness_with(crate::common::builder().with_os(OperatingSystem::Nix).with_size(egui::vec2(1200.0, 800.0)), app, 4)
}

fn key(h: &mut Harness, k: Key, m: Modifiers) {
    h.input_mut().events.push(Event::Key { key: k, physical_key: Some(k), pressed: true, repeat: false, modifiers: m });
    h.input_mut().events.push(Event::Key { key: k, physical_key: Some(k), pressed: false, repeat: false, modifiers: m });
    h.run_steps(2);
}

/// A keytip letter, as a keyboard sends it (the key, then its text).
fn letter(h: &mut Harness, k: Key) {
    h.input_mut().events.push(Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
    h.input_mut().events.push(Event::Text(k.name().to_ascii_lowercase()));
    h.input_mut().events.push(Event::Key { key: k, physical_key: None, pressed: false, repeat: false, modifiers: Modifiers::NONE });
    h.run_steps(2);
}

/// (number of open workbooks, the active workbook's uid).
fn documents(h: &Harness) -> (usize, Option<u64>) {
    (h.state().session.documents().len(), h.state().session.active().map(|d| d.uid))
}

fn swaps_journaled(h: &Harness) -> Vec<String> {
    h.state().session.journal.iter().map(|(id, _)| id.clone()).filter(|id| matches!(id.as_str(), "file.new" | "file.open" | "file.close")).collect()
}

/// A workbook with `value` in A1, as `.xlsx` bytes.
fn workbook_bytes(value: &str) -> Vec<u8> {
    let mut s = Session::new();
    s.new_workbook();
    s.run("cell.set", json!({"cell": "A1", "input": value})).unwrap();
    gridcraft_engine::io::save_bytes(&s.active().unwrap().wb, "book.xlsx").unwrap()
}

fn a1(h: &Harness) -> Value {
    h.state().session.active().unwrap().wb.active().unwrap().value(CellRef::parse("A1").unwrap())
}

#[test]
fn locked_shortcuts_do_not_new_open_or_close() {
    let mut h = harness(true, Services::default());
    let before = documents(&h);
    for k in [Key::N, Key::O, Key::W] {
        key(&mut h, k, CTRL);
        assert_eq!(documents(&h), before, "Ctrl+{} leaves the workbook as it was", k.name());
    }
    assert!(swaps_journaled(&h).is_empty(), "no New, Open or Close ran: {:?}", swaps_journaled(&h));
    assert!(h.state().message.is_none(), "refused quietly, no message box");
}

#[test]
fn locked_run_refuses_document_swaps() {
    let mut h = harness(true, Services::default());
    let before = documents(&h);
    for (id, params) in [
        ("file.new", json!({})),
        ("file.new", json!({"sample": "sales"})),
        ("file.open", json!({"name": "Other.xlsx", "base64": gridcraft_engine::io::base64_encode(&workbook_bytes("7"))})),
        ("file.close", json!({"force": true})),
        ("window.close", json!({"force": true})),
    ] {
        assert_eq!(h.state_mut().run(id, params), Err("not available here".to_string()), "{id} is refused");
        h.run_steps(2);
        assert_eq!(documents(&h), before, "{id} leaves the workbook as it was");
    }
    // A second window on the same workbook can still close: the workbook stays.
    assert!(h.state_mut().run("view.newWindow", json!({})).is_ok());
    assert!(h.state_mut().run("window.close", json!({})).is_ok(), "closing one of two windows is allowed");
    assert_eq!(documents(&h), before);
}

#[test]
fn locked_keytips_do_not_open_backstage() {
    let mut h = harness(true, Services::default());
    let before = documents(&h);
    key(&mut h, Key::F10, Modifiers::NONE);
    letter(&mut h, Key::F);
    assert!(h.state().backstage.is_none(), "Alt, F doesn't open the File view");
    for k in [Key::N, Key::O, Key::C] {
        letter(&mut h, k);
        assert!(h.state().backstage.is_none());
        assert_eq!(documents(&h), before);
    }
    assert!(swaps_journaled(&h).is_empty(), "{:?}", swaps_journaled(&h));
}

/// Services with the person's inbox and the host's queue.
fn queues() -> (Services, Inbox, Inbox) {
    let (inbox, host_open): (Inbox, Inbox) = Default::default();
    (Services { inbox: Some(inbox.clone()), host_open: Some(host_open.clone()), ..Default::default() }, inbox, host_open)
}

fn dialog_name(h: &Harness) -> Option<String> {
    h.state().dialog.as_ref().map(|d| d.name().to_string())
}

/// Types the password into the open password dialog and presses OK.
fn enter_password(h: &mut Harness, pw: &str) {
    assert_eq!(dialog_name(h).as_deref(), Some("openPassword"));
    h.state_mut().dialog.as_mut().unwrap().values.insert("password".into(), json!(pw));
    h.get_by_label("   OK   ").click();
    h.run_steps(3);
}

#[test]
fn locked_host_open_still_opens() {
    let (services, _, host_open) = queues();
    let mut h = harness(true, services);
    host_open.lock().unwrap().push(("Stored.xlsx".into(), workbook_bytes("42")));
    h.run_steps(3);
    assert!(h.state().message.is_none(), "{:?}", h.state().message);
    assert_eq!(a1(&h), Value::Number(42.0), "the host's file is the open workbook");
    assert_eq!(documents(&h).0, 1, "the untouched blank made way");
}

#[test]
fn locked_host_open_still_asks_for_a_password() {
    let (services, _, host_open) = queues();
    let mut h = harness(true, services);
    let pw = "synthetic-pw-5150";
    host_open.lock().unwrap().push(("Stored.xlsx".into(), gridcraft_xlsx::testing::encrypt_workbook(&workbook_bytes("42"), pw)));
    h.run_steps(3);
    enter_password(&mut h, pw);
    assert!(h.state().message.is_none(), "{:?}", h.state().message);
    assert_eq!(a1(&h), Value::Number(42.0), "the host's protected file opens with its password");
}

#[test]
fn locked_host_open_replaces_an_edited_blank() {
    let (services, _, host_open) = queues();
    let mut h = harness(true, services);
    h.state_mut().run("cell.set", json!({"cell": "A1", "input": "typed first"})).unwrap();
    h.run_steps(2);
    assert!(h.state().session.active().unwrap().is_dirty());
    host_open.lock().unwrap().push(("Stored.xlsx".into(), workbook_bytes("42")));
    h.run_steps(3);
    assert_eq!(documents(&h).0, 1, "the edited blank closed: the host's workbook is the only one");
    assert_eq!(a1(&h), Value::Number(42.0));
    assert!(h.state().message.is_none(), "{:?}", h.state().message);
    assert_eq!(dialog_name(&h), None, "closed without asking to save");
}

#[test]
fn locked_host_open_behind_a_password_replaces_an_edited_blank() {
    let (services, _, host_open) = queues();
    let mut h = harness(true, services);
    h.state_mut().run("cell.set", json!({"cell": "A1", "input": "typed first"})).unwrap();
    h.run_steps(2);
    let pw = "synthetic-pw-5150";
    host_open.lock().unwrap().push(("Stored.xlsx".into(), gridcraft_xlsx::testing::encrypt_workbook(&workbook_bytes("42"), pw)));
    h.run_steps(3);
    enter_password(&mut h, "wrong");
    assert!(h.state_mut().message.take().is_some(), "a wrong password says so");
    enter_password(&mut h, pw);
    assert!(h.state().message.is_none(), "{:?}", h.state().message);
    assert_eq!(documents(&h).0, 1, "the edited blank closed once the host's workbook opened");
    assert_eq!(a1(&h), Value::Number(42.0));
}

#[test]
fn locked_picker_workbook_is_refused() {
    let (services, inbox, _) = queues();
    let mut h = harness(true, services);
    let before = documents(&h);
    inbox.lock().unwrap().push(("Picked.xlsx".into(), workbook_bytes("7")));
    inbox.lock().unwrap().push(("Picked.csv".into(), b"1,2,3".to_vec()));
    inbox.lock().unwrap().push(("Protected.xlsx".into(), gridcraft_xlsx::testing::encrypt_workbook(&workbook_bytes("7"), "pw")));
    h.run_steps(3);
    assert_eq!(documents(&h), before, "a picked or dropped workbook does not open");
    assert!(swaps_journaled(&h).is_empty(), "{:?}", swaps_journaled(&h));
    assert!(h.state().message.is_none(), "refused quietly: {:?}", h.state().message);
    assert_eq!(dialog_name(&h), None, "not even a password dialog");
    // A password dialog not raised by the host's own file can't open one either.
    let b64 = gridcraft_engine::io::base64_encode(&gridcraft_xlsx::testing::encrypt_workbook(&workbook_bytes("7"), "pw"));
    h.state_mut().open_dialog("openPassword", json!({"name": "Protected.xlsx", "base64": b64}));
    h.run_steps(2);
    enter_password(&mut h, "pw");
    assert_eq!(documents(&h), before, "the password dialog's open is refused");
}

#[test]
fn locked_get_data_and_start_open_do_nothing() {
    for locked in [false, true] {
        let picks = std::rc::Rc::new(std::cell::Cell::new(0));
        let counter = picks.clone();
        let services = Services { open_async: Some(Box::new(move || counter.set(counter.get() + 1))), ..Default::default() };
        let mut h = harness(locked, services);
        // What Data > Get Data (Text/CSV) and the start page's Open... do.
        h.state_mut().open_dialog("open", json!({}));
        h.run_steps(2);
        // Get Data from command search or a keytip asks the engine, which asks for the picker.
        h.state_mut().run_or_alert("data.getData", json!({}));
        h.run_steps(2);
        assert_eq!(picks.get(), if locked { 0 } else { 2 }, "file pickers started (locked: {locked})");
        assert!(h.state().message.is_none(), "{:?}", h.state().message);
        // Home opens the start page; locked, it has no Open….
        h.get_by_label("Home").click();
        h.run_steps(2);
        assert_eq!(dialog_name(&h).as_deref(), Some("start"));
        assert_eq!(h.query_by_label("Open…").is_some(), !locked, "start page's Open… (locked: {locked})");
    }
}

#[test]
fn locked_title_bar_leaves_out_new_and_open() {
    for (locked, listed) in [(false, true), (true, false)] {
        let mut h = harness(locked, Services::default());
        h.get_by_label("More commands").click();
        h.run_steps(2);
        assert!(h.query_by_label("Save As…").is_some(), "the menu is open");
        assert_eq!(h.query_by_label("New Workbook").is_some(), listed, "locked: {locked}");
        assert_eq!(h.query_by_label("Open…").is_some(), listed, "locked: {locked}");
    }
}

#[test]
fn locked_save_still_runs() {
    let mut h = harness(true, Services::default());
    let r = h.state_mut().run("file.save", json!({}));
    assert_ne!(r, Err("not available here".to_string()), "Save is not refused");
    let r = h.state_mut().run("file.saveBytes", json!({"format": "xlsx"}));
    assert!(r.is_ok(), "the bytes a host stores still come back: {r:?}");
}

#[test]
fn locked_hides_the_file_tab() {
    let mut h = harness(true, Services::default());
    assert!(!h.state().chrome.file_tab, "a locked page has no File tab");
    h.state_mut().open_backstage(gridcraft_ui_egui::backstage::Page::Open);
    h.run_steps(2);
    assert!(h.state().backstage.is_none(), "and Backstage can't open");
}

#[test]
fn unlocked_new_still_works() {
    let mut h = harness(false, Services::default());
    let (count, _) = documents(&h);
    key(&mut h, Key::N, CTRL);
    assert_eq!(documents(&h).0, count + 1, "Ctrl+N opens a new workbook");
    assert!(h.state_mut().run("file.new", json!({})).is_ok());
    assert_eq!(documents(&h).0, count + 2);
    assert!(h.state().chrome.file_tab, "the File tab stays");
    let (services, inbox, host_open) = queues();
    let mut h = harness(false, services);
    inbox.lock().unwrap().push(("Dropped.xlsx".into(), workbook_bytes("9")));
    h.run_steps(3);
    assert_eq!(a1(&h), Value::Number(9.0), "Open still opens");
    // Unlocked, a host's workbook opens as a picked one does: beside an edited workbook.
    h.state_mut().run("cell.set", json!({"cell": "B1", "input": "edited"})).unwrap();
    let (count, _) = documents(&h);
    host_open.lock().unwrap().push(("Stored.xlsx".into(), workbook_bytes("42")));
    h.run_steps(3);
    assert_eq!(documents(&h).0, count + 1);
    assert_eq!(a1(&h), Value::Number(42.0));
}
