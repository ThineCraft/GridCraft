//! A host that owns the open file can lock it: the person can't swap it for another workbook
//! (New, Open, Close), while the host's own open and the person's Save keep working.

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

#[test]
fn locked_host_open_still_opens() {
    let inbox: Inbox = Default::default();
    let mut h = harness(true, Services { inbox: Some(inbox.clone()), ..Default::default() });
    inbox.lock().unwrap().push(("Stored.xlsx".into(), workbook_bytes("42")));
    h.run_steps(3);
    assert!(h.state().message.is_none(), "{:?}", h.state().message);
    assert_eq!(a1(&h), Value::Number(42.0), "the host's file is the open workbook");
}

#[test]
fn locked_host_open_still_asks_for_a_password() {
    let inbox: Inbox = Default::default();
    let mut h = harness(true, Services { inbox: Some(inbox.clone()), ..Default::default() });
    let pw = "synthetic-pw-5150";
    inbox.lock().unwrap().push(("Stored.xlsx".into(), gridcraft_xlsx::testing::encrypt_workbook(&workbook_bytes("42"), pw)));
    h.run_steps(3);
    assert_eq!(h.state().dialog.as_ref().map(|d| d.name().to_string()).as_deref(), Some("openPassword"));
    h.state_mut().dialog.as_mut().unwrap().values.insert("password".into(), json!(pw));
    h.get_by_label("   OK   ").click();
    h.run_steps(3);
    assert!(h.state().message.is_none(), "{:?}", h.state().message);
    assert_eq!(a1(&h), Value::Number(42.0), "the host's protected file opens with its password");
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
    let inbox: Inbox = Default::default();
    let mut h = harness(false, Services { inbox: Some(inbox.clone()), ..Default::default() });
    inbox.lock().unwrap().push(("Dropped.xlsx".into(), workbook_bytes("9")));
    h.run_steps(3);
    assert_eq!(a1(&h), Value::Number(9.0), "Open still opens");
}
