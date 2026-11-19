//! Live mode on the screen: while a workbook is edited together, the app's commands go through
//! the live seat ([`LiveLink`]) instead of straight to the session.
//!
//! The app keeps its `session` field, so everything that reads the shown workbook is unchanged.
//! The host lends the shared session to the app for each frame and, for each live call, the link
//! moves it into the coauthor and back (a move of the struct, never a copy).
//!
//! - Shared commands are made explicit on the sender's side ([`explicit_params`]) and sent;
//!   personal ones (selection, sheet switching, zoom, frozen panes, copy, find) run as usual;
//!   everything else is refused with a notice. Undo and Redo use the person's own history.
//! - A cell draft stays private until Enter, then becomes one `cell.set`.
//! - The screen's own direct writes are switched off: the view state is not stored into the
//!   workbook, the filter menu's on-demand table filter is not installed, and the form-control
//!   and object-drag undo trimming is skipped. The session refuses any other command run on it
//!   directly while it is shared.
//! - Save does not hand over a file: checkpoints are automatic.
//! - When the person may only view, edits are refused before anything is shown.
//! - Other people's places are drawn on the grid ([`paint_places`]).

use std::fmt;

use egui::{Color32, Painter, Rect, Stroke, StrokeKind, pos2, vec2};
use gridcraft_engine::Session;
use gridcraft_engine::coauthor::{Route, explicit_params, route};
use gridcraft_engine::core::RangeRef;
use serde_json::Value as Json;

use crate::{SheetApp, now_ms};

/// Why an edit is refused when the person may only view.
pub const READ_ONLY_REASON: &str = "You can view this workbook but not change it while editing together";

/// What Save says while editing together.
pub const SAVED_BY_ROOM: &str = "Changes are saved automatically while editing together";

/// The live seat as the screen sees it. The host implements it over its live client.
pub trait LiveLink {
    /// Whether the person may make shared changes now: the page and the room both allow it.
    fn may_edit(&self) -> bool;
    /// Sends one shared change with explicit targets. `session` is the shown session, moved into
    /// the coauthor for the call and back.
    fn submit(&self, session: &mut Session, id: &str, params: Json, origin_year: i32) -> Result<(), String>;
    /// The person's own Undo.
    fn undo(&self, session: &mut Session) -> Result<(), String>;
    /// The person's own Redo.
    fn redo(&self, session: &mut Session) -> Result<(), String>;
    /// Whether the person is typing in a cell; shown to others as a mark, never as text.
    fn set_editing(&self, editing: bool);
    /// Other people's places, resolved against the shown workbook.
    fn others(&self) -> Vec<OtherPlace>;
}

/// Another person's place to draw.
#[derive(Clone, PartialEq)]
pub struct OtherPlace {
    pub participant: u32,
    pub name: String,
    pub initials: String,
    pub colour: Color32,
    /// Whether their client is connected now; a place without a connection is not drawn.
    pub connected: bool,
    /// Whether they may edit; a viewer is outlined dashed.
    pub can_edit: bool,
    pub sheet: usize,
    /// Their active cell, zero-based (row, column).
    pub active: (u32, u32),
    pub range: Option<RangeRef>,
    /// Whether they are typing in their active cell.
    pub editing: bool,
}

/// Debug leaves the name and initials out.
impl fmt::Debug for OtherPlace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtherPlace")
            .field("participant", &self.participant)
            .field("connected", &self.connected)
            .field("sheet", &self.sheet)
            .field("active", &self.active)
            .finish()
    }
}

/// The year two-digit years are read against, from the clock.
fn origin_year() -> i32 {
    let days = (gridcraft_engine::core::date::unix_now_ms().unwrap_or(0.0) / 86_400_000.0).floor();
    let days = if days.is_finite() { days as i64 } else { 0 };
    gridcraft_engine::core::date::civil_from_days(days).0
}

impl SheetApp {
    /// Whether the shown workbook is edited together.
    pub fn is_live(&self) -> bool {
        self.live.is_some()
    }

    /// Whether a change may be made here: always outside live mode.
    pub(crate) fn live_may_edit(&self) -> bool {
        self.live.as_ref().is_none_or(|link| link.may_edit())
    }

    /// Tells others whether the person is typing (live mode only).
    pub(crate) fn live_editing(&self, editing: bool) {
        if let Some(link) = &self.live {
            link.set_editing(editing);
        }
    }

    /// Runs `id` through the live seat. `None` outside live mode, and for commands that run as
    /// usual: personal ones, UI commands and exports.
    pub(crate) fn run_live(&mut self, id: &str, params: &Json) -> Option<Result<Json, String>> {
        let link = self.live.as_deref()?;
        let result = match id {
            "file.save" | "file.saveAs" => {
                self.toast = Some((SAVED_BY_ROOM.into(), now_ms()));
                return Some(Ok(Json::Null));
            }
            "edit.undo" | "edit.redo" if !link.may_edit() => Err(READ_ONLY_REASON.to_string()),
            "edit.undo" => link.undo(&mut self.session),
            "edit.redo" => link.redo(&mut self.session),
            _ => {
                gridcraft_engine::find_command(id)?;
                match route(id) {
                    Route::Local => return None,
                    Route::Refused(reason) => Err(reason.to_string()),
                    Route::Shared if !link.may_edit() => Err(READ_ONLY_REASON.to_string()),
                    Route::Shared => match explicit_params(&self.session, id, params.clone()) {
                        Ok(explicit) => link.submit(&mut self.session, id, explicit, origin_year()),
                        Err(error) => Err(error.to_string()),
                    },
                }
            }
        };
        if result.is_ok() {
            self.after_engine();
        }
        Some(result.map(|()| Json::Null))
    }

    /// Runs an engine command the screen makes itself (a committed cell draft): through the live
    /// seat while editing together, straight on the session otherwise.
    pub(crate) fn run_engine(&mut self, id: &str, params: Json) -> Result<Json, String> {
        match self.run_live(id, &params) {
            Some(result) => result,
            None => self.session.run(id, params),
        }
    }
}

/// The text colour for a tag on `background`: black or white, whichever reads better.
fn contrast(background: Color32) -> Color32 {
    let luma = 0.299 * f32::from(background.r()) + 0.587 * f32::from(background.g()) + 0.114 * f32::from(background.b());
    if luma > 150.0 { Color32::BLACK } else { Color32::WHITE }
}

/// Draws the connected people's places on sheet `sheet`: a 2-point outline round their selection
/// and active cell in their colour (dashed for a viewer), a tag with their initials at the active
/// cell's top-left (their name on hover), and a pencil mark while they type. `rect_of` gives a
/// range's rectangle on screen.
pub fn paint_places(p: &Painter, places: &[OtherPlace], sheet: usize, rect_of: &dyn Fn(RangeRef) -> Rect) {
    for place in places.iter().filter(|place| place.connected && place.sheet == sheet) {
        let active = rect_of(RangeRef::cell(gridcraft_engine::core::CellRef::new(place.active.0, place.active.1)));
        let stroke = Stroke::new(2.0, place.colour);
        for area in place.range.map(rect_of).into_iter().chain(std::iter::once(active)) {
            if !area.is_finite() {
                continue;
            }
            if place.can_edit {
                p.rect_stroke(area, 0.0, stroke, StrokeKind::Inside);
            } else {
                let corners = [area.left_top(), area.right_top(), area.right_bottom(), area.left_bottom(), area.left_top()];
                p.extend(egui::Shape::dashed_line(&corners, stroke, 4.0, 3.0));
            }
        }
        if !active.is_finite() {
            continue;
        }
        let font = egui::FontId::proportional(10.0);
        let galley = p.layout_no_wrap(place.initials.clone(), font.clone(), contrast(place.colour));
        let tag = Rect::from_min_size(pos2(active.left(), active.top() - galley.size().y - 2.0), galley.size() + vec2(6.0, 2.0));
        p.rect_filled(tag, 2.0, place.colour);
        p.galley(tag.min + vec2(3.0, 1.0), galley, contrast(place.colour));
        if place.editing {
            // A small pencil drawn as a shape, so no font needs the glyph.
            let tip = pos2(tag.right() + 3.0, tag.bottom());
            let pencil = Stroke::new(1.5, place.colour);
            p.line_segment([tip, tip + vec2(6.0, -6.0)], pencil);
            p.line_segment([tip, tip + vec2(2.0, 0.0)], pencil);
        }
        if p.ctx().pointer_hover_pos().is_some_and(|pointer| tag.contains(pointer)) {
            let name = p.layout_no_wrap(place.name.clone(), font, contrast(place.colour));
            let label = Rect::from_min_size(pos2(tag.left(), tag.top() - name.size().y - 4.0), name.size() + vec2(6.0, 2.0));
            p.rect_filled(label, 2.0, place.colour);
            p.galley(label.min + vec2(3.0, 1.0), name, contrast(place.colour));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    use gridcraft_engine::Session;
    use gridcraft_engine::coauthor::{GRIDCRAFT_LIVE_FORMAT, GridCoauthor, GridLiveEditor, LiveConfig, Route, route};
    use gridcraft_engine::core::{CellRef, RangeRef, Value};
    use gridcraft_engine::fingerprint::{DocId, fingerprint};
    use serde_json::{Value as Json, json};

    use super::*;
    use crate::{Services, SheetApp};

    /// A test seat: the coauthor, the frames it sent, and what the page and room allow.
    struct Seat {
        coauthor: RefCell<GridCoauthor>,
        sent: RefCell<Vec<Json>>,
        page_may_edit: bool,
        editing: Cell<bool>,
        others: Vec<OtherPlace>,
        clock: Cell<u64>,
    }

    impl Seat {
        fn tick(&self) -> u64 {
            let now = self.clock.get() + 100;
            self.clock.set(now);
            now
        }

        fn keep(&self, frames: Vec<String>) {
            self.sent.borrow_mut().extend(frames.iter().filter_map(|frame| serde_json::from_str::<Json>(frame).ok()));
        }

        fn entries(&self) -> Vec<Json> {
            self.sent.borrow().iter().filter(|frame| frame["type"] == "entry").cloned().collect()
        }

        /// The server's ordered copy of this seat's `n`th entry, delivered back to it.
        fn confirm(&self, session: &mut Session, n: usize, seq: u64) {
            let entries = self.entries();
            let sent = &entries[n];
            let client = self.client();
            let frame = json!({
                "type":"entry",
                "entry":{"seq":seq,"at_ms":1_795_219_200_000_i64,"seed":7,"kind":"change","participant":2,"client":client,
                    "local":sent["local"],"seen":sent["seen"],"payload":sent["payload"],"cancelled":false}
            });
            let now = self.tick();
            lend(session, &self.coauthor, |coauthor| {
                let step = coauthor.receive(&frame.to_string(), now).unwrap();
                self.keep(step.send);
            });
        }

        fn client(&self) -> Json {
            self.sent.borrow().iter().find(|frame| frame["type"] == "live_join").map(|frame| frame["client"].clone()).unwrap()
        }
    }

    /// Runs `f` with the shown session moved into the coauthor and back, as the web seat does.
    fn lend<T>(session: &mut Session, coauthor: &RefCell<GridCoauthor>, f: impl FnOnce(&mut GridCoauthor) -> T) -> T {
        let mut coauthor = coauthor.borrow_mut();
        std::mem::swap(session, coauthor.editor_mut().session_mut());
        let result = f(&mut coauthor);
        std::mem::swap(session, coauthor.editor_mut().session_mut());
        result
    }

    struct Link(Rc<Seat>);

    impl LiveLink for Link {
        fn may_edit(&self) -> bool {
            self.0.page_may_edit && self.0.coauthor.borrow().can_edit()
        }

        fn submit(&self, session: &mut Session, id: &str, params: Json, origin_year: i32) -> Result<(), String> {
            let now = self.0.tick();
            let step = lend(session, &self.0.coauthor, |coauthor| coauthor.submit(id, params, origin_year, now)).map_err(|e| e.to_string())?;
            self.0.keep(step.send);
            Ok(())
        }

        fn undo(&self, session: &mut Session) -> Result<(), String> {
            let now = self.0.tick();
            let step = lend(session, &self.0.coauthor, |coauthor| coauthor.undo(now)).map_err(|e| e.to_string())?;
            self.0.keep(step.send);
            Ok(())
        }

        fn redo(&self, session: &mut Session) -> Result<(), String> {
            let now = self.0.tick();
            let step = lend(session, &self.0.coauthor, |coauthor| coauthor.redo(now)).map_err(|e| e.to_string())?;
            self.0.keep(step.send);
            Ok(())
        }

        fn set_editing(&self, editing: bool) {
            self.0.editing.set(editing);
        }

        fn others(&self) -> Vec<OtherPlace> {
            self.0.others.clone()
        }
    }

    fn data_session() -> Session {
        let mut session = Session::new();
        let mut workbook = gridcraft_engine::model::Workbook::new();
        workbook.sheets.clear();
        workbook.sheets.push(Arc::new(gridcraft_engine::model::Sheet::new("Data")));
        session.add_document(gridcraft_engine::DocState::new(workbook, None, "Book1".into(), gridcraft_engine::Trust::Local));
        session
    }

    /// A live app: the coauthor joined its room as participant 2, its session lent to the app.
    fn live_app(page_may_edit: bool, room_may_edit: bool, others: Vec<OtherPlace>) -> (SheetApp, Rc<Seat>, Arc<Mutex<Vec<String>>>) {
        live_app_with(data_session(), page_may_edit, room_may_edit, others)
    }

    fn live_app_with(
        session: Session,
        page_may_edit: bool,
        room_may_edit: bool,
        others: Vec<OtherPlace>,
    ) -> (SheetApp, Rc<Seat>, Arc<Mutex<Vec<String>>>) {
        let config = LiveConfig { format: GRIDCRAFT_LIVE_FORMAT.to_string(), crashed_at: None };
        let mut coauthor = GridCoauthor::new(config.clone(), GridLiveEditor::new(session).unwrap());
        let seat = Rc::new(Seat {
            coauthor: RefCell::new(GridCoauthor::new(config, GridLiveEditor::blank().unwrap())),
            sent: RefCell::default(),
            page_may_edit,
            editing: Cell::new(false),
            others,
            clock: Cell::new(0),
        });
        seat.keep(coauthor.tick(1));
        let joined = json!({"type":"live_joined","session":"00000000-0000-0000-0000-00000000005e","participant":2,"place":{"is":"new"},
            "can_edit":room_may_edit,"heartbeat_ms":60_000,"people":[]});
        coauthor.receive(&joined.to_string(), 2).unwrap();
        coauthor.receive(&json!({"type":"catchup","base":0,"revision":null,"from":0,"head":0}).to_string(), 3).unwrap();
        coauthor.base_ready(4).unwrap();
        *seat.coauthor.borrow_mut() = coauthor;
        seat.clock.set(10);
        let downloads = Arc::new(Mutex::new(Vec::new()));
        let saved = Arc::clone(&downloads);
        let services = Services {
            download: Some(Box::new(move |name: &str, _bytes: &[u8]| saved.lock().unwrap().push(name.to_string()))),
            ..Services::default()
        };
        let mut app = SheetApp::new(Session::new(), services);
        std::mem::swap(&mut app.session, seat.coauthor.borrow_mut().editor_mut().session_mut());
        app.live = Some(Box::new(Link(Rc::clone(&seat))));
        (app, seat, downloads)
    }

    fn entered(app: &SheetApp) -> [u8; 32] {
        fingerprint(&app.session, DocId(0)).entered
    }

    fn value(app: &SheetApp, cell: &str) -> Value {
        app.session.doc().unwrap().wb.sheet(0).unwrap().value(CellRef::parse(cell).unwrap())
    }

    fn bold(app: &SheetApp, cell: &str) -> bool {
        let wb = &app.session.doc().unwrap().wb;
        let sheet = wb.sheet(0).unwrap();
        wb.styles.get(sheet.style_id(CellRef::parse(cell).unwrap())).font.bold
    }

    #[test]
    fn live_mode_sends_shared_commands_through_the_coauthor() {
        let (mut app, seat, _) = live_app(true, true, vec![]);
        app.run("selection.set", json!({"range":"B2"})).unwrap();
        app.run("home.bold", json!({})).unwrap();
        let entries = seat.entries();
        assert_eq!(entries.len(), 1, "one entry for the change");
        assert!(bold(&app, "B2"), "shown at once");
        assert_eq!(seat.coauthor.borrow().waiting(), 1, "it waits for the server");
    }

    #[test]
    fn live_mode_runs_view_commands_locally() {
        let (mut app, seat, _) = live_app(true, true, vec![]);
        let before = entered(&app);
        app.run("selection.set", json!({"range":"C3"})).unwrap();
        app.run("view.zoom", json!({"percent":150})).unwrap();
        assert!(seat.entries().is_empty(), "personal view work is never sent");
        assert_eq!(app.session.doc().unwrap().selection.active, CellRef::parse("C3").unwrap());
        assert_eq!(entered(&app), before);
        assert_eq!(route("view.zoom"), Route::Local);
    }

    #[test]
    fn live_mode_refuses_unshared_commands_with_a_notice() {
        let (mut app, seat, _) = live_app(true, true, vec![]);
        let before = entered(&app);
        assert!(app.run("home.mergeCells", json!({"range":"A1:B2"})).is_err());
        app.run_or_alert("home.mergeCells", json!({"range":"A1:B2"}));
        assert!(app.toast.as_ref().is_some_and(|(text, _)| text.contains("editing together")), "the person is told why: {:?}", app.toast);
        assert!(seat.entries().is_empty());
        assert_eq!(entered(&app), before, "nothing changed");
        // A command run straight on the shared session is refused too.
        assert!(app.session.run("home.mergeCells", json!({"range":"A1:B2"})).is_err());
        assert_eq!(entered(&app), before);
    }

    #[test]
    fn live_undo_and_redo_use_personal_history() {
        let (mut app, seat, _) = live_app(true, true, vec![]);
        app.run("cell.set", json!({"cell":"A1","input":"5"})).unwrap();
        seat.confirm(&mut app.session, 0, 1);
        assert_eq!(value(&app, "A1"), Value::Number(5.0));
        app.run("edit.undo", json!({})).unwrap();
        assert_eq!(seat.entries().len(), 2, "Undo is sent as a change of its own");
        assert_eq!(value(&app, "A1"), Value::Empty, "and shown at once");
        seat.confirm(&mut app.session, 1, 2);
        app.run("edit.redo", json!({})).unwrap();
        assert_eq!(seat.entries().len(), 3);
        assert_eq!(value(&app, "A1"), Value::Number(5.0));
    }

    #[test]
    fn filter_menu_goes_through_the_entry_point_when_live() {
        let mut session = data_session();
        session.execute("cell.set", json!({"cell":"A1","input":"Kind"})).unwrap();
        session.execute("cell.set", json!({"cell":"A2","input":"x"})).unwrap();
        session.execute("data.filterSet", json!({"sheet":0,"range":"A1:A3"})).unwrap();
        let (mut app, seat, _) = live_app_with(session, true, true, vec![]);
        app.run("selection.set", json!({"range":"D9"})).unwrap();
        app.run("data.filterBy", json!({"column":"A","values":["x"]})).unwrap();
        let entries = seat.entries();
        assert_eq!(entries.len(), 1, "the filter menu's command is a shared change");
        let payload: Json = serde_json::from_str(entries[0]["payload"].as_str().unwrap()).unwrap();
        assert_eq!(payload["command"]["id"], "data.filterBy");

        // The menu's on-demand table filter is a direct write: not made while live.
        let before = entered(&app);
        crate::grid::menus::install_table_filter(&mut app, RangeRef::parse("A1:A3").unwrap(), 0);
        assert_eq!(entered(&app), before);
    }

    #[test]
    fn control_and_object_drags_do_not_trim_shared_history() {
        let (mut app, _seat, _) = live_app(true, true, vec![]);
        let undo = |app: &SheetApp| app.session.active().unwrap().undo.len();
        app.session.active_mut().unwrap().undo.clear();
        for _ in 0..3 {
            app.session.execute("cell.set", json!({"cell":"Z1","input":"1"})).unwrap();
        }
        let before = undo(&app);
        crate::grid::interact::coalesce_last_undo(&mut app);
        crate::form_controls::trim_control_undo(&mut app, 0);
        assert_eq!(undo(&app), before, "no shared undo entry is merged or dropped by the screen");
    }

    #[test]
    fn live_view_state_is_not_written_into_the_workbook() {
        let (mut app, _seat, _) = live_app(true, true, vec![]);
        let window = app.session.active_window().unwrap();
        let sheet = app.session.doc().unwrap().wb.sheet(0).unwrap().uid.get();
        app.views.insert((window, sheet), crate::SheetView { scroll: egui::vec2(400.0, 900.0) });
        let before = Arc::clone(&app.session.doc().unwrap().wb);
        app.store_view_state();
        assert!(Arc::ptr_eq(&before, &app.session.doc().unwrap().wb), "the person's scroll is not written into the shared workbook");
    }

    #[test]
    fn live_save_does_not_upload_a_file() {
        let (mut app, seat, downloads) = live_app(true, true, vec![]);
        app.run("file.save", json!({})).unwrap();
        assert!(downloads.lock().unwrap().is_empty(), "no file is handed over");
        assert!(app.toast.is_some(), "the person is told changes are saved automatically");
        assert!(seat.entries().is_empty());
    }

    #[test]
    fn read_only_live_mode_refuses_edits_and_keeps_navigation() {
        for (page, room) in [(false, true), (true, false)] {
            let (mut app, seat, _) = live_app(page, room, vec![]);
            let before = entered(&app);
            assert!(app.run("home.bold", json!({"range":"A1"})).is_err());
            app.begin_edit(Some("typed".into()), false);
            assert!(app.editor.is_none(), "the cell editor does not open");
            assert!(app.run("edit.undo", json!({})).is_err());
            app.run("selection.set", json!({"range":"D4"})).unwrap();
            app.run("edit.copy", json!({})).unwrap();
            assert_eq!(app.session.doc().unwrap().selection.active, CellRef::parse("D4").unwrap());
            assert!(seat.entries().is_empty());
            assert_eq!(entered(&app), before);
        }
    }

    #[test]
    fn cell_draft_is_sent_only_on_enter() {
        let (mut app, seat, _) = live_app(true, true, vec![]);
        app.begin_edit(Some("4".into()), false);
        assert!(seat.editing.get(), "typing is shown as a mark, never as text");
        if let Some(editor) = app.editor.as_mut() {
            editor.text = "42".into();
        }
        assert!(seat.entries().is_empty(), "the draft stays private");
        assert!(app.commit_edit(1, 0, false, false));
        let entries = seat.entries();
        assert_eq!(entries.len(), 1, "Enter sends one change");
        let payload: Json = serde_json::from_str(entries[0]["payload"].as_str().unwrap()).unwrap();
        assert_eq!(payload["command"]["id"], "cell.set");
        assert!(!seat.editing.get());
        assert_eq!(value(&app, "A1"), Value::Number(42.0));
    }

    fn place(connected: bool, can_edit: bool, editing: bool) -> OtherPlace {
        OtherPlace {
            participant: 3,
            name: "Ann Lee".into(),
            initials: "AL".into(),
            colour: egui::Color32::from_rgb(200, 40, 40),
            connected,
            can_edit,
            sheet: 0,
            active: (1, 1),
            range: Some(RangeRef::parse("B2:C4").unwrap()),
            editing,
        }
    }

    /// The shapes painted for `places` on sheet 0, with every cell 10 by 10 points.
    fn painted(places: &[OtherPlace]) -> Vec<egui::epaint::ClippedShape> {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let painter = ui.ctx().layer_painter(egui::LayerId::background());
            let rect_of = |range: RangeRef| {
                egui::Rect::from_min_max(
                    egui::pos2(range.start.col as f32 * 10.0, range.start.row as f32 * 10.0),
                    egui::pos2((range.end.col + 1) as f32 * 10.0, (range.end.row + 1) as f32 * 10.0),
                )
            };
            paint_places(&painter, places, 0, &rect_of);
        });
        output.textures_delta.clear();
        output.shapes
    }

    fn texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<String> {
        shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn others_selections_are_drawn_with_initials() {
        let shapes = painted(&[place(true, true, true)]);
        assert!(texts(&shapes).iter().any(|text| text == "AL"), "the initials tag is drawn");
        let coloured = shapes.iter().any(|shape| match &shape.shape {
            egui::Shape::Rect(rect) => rect.stroke.color == egui::Color32::from_rgb(200, 40, 40) && rect.stroke.width >= 2.0,
            _ => false,
        });
        assert!(coloured, "a 2-point outline in the person's colour");
        assert!(painted(&[place(true, true, true)]).len() > painted(&[place(true, true, false)]).len(), "a pencil mark only while typing");
        let viewer = painted(&[place(true, false, false)]);
        assert!(viewer.iter().any(|shape| matches!(shape.shape, egui::Shape::LineSegment { .. } | egui::Shape::Vec(_))), "a viewer is drawn dashed");
    }

    #[test]
    fn disconnected_places_are_not_drawn() {
        assert!(painted(&[place(false, true, false)]).is_empty(), "nothing for a person without a connection");
        let mut elsewhere = place(true, true, false);
        elsewhere.sheet = 1;
        assert!(painted(&[elsewhere]).is_empty(), "nor for one on another sheet");
    }
}
