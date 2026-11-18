use std::collections::BTreeMap;
use std::sync::Arc;

use collab_core::ids::Seq;
use collab_live::client::{Crash, Effect, EntryView, LiveEditor, Outcome, Stamp};
use collab_live::ids::{IdError, LiveId};
use serde_json::Value as Json;

use super::{CoauthorError, LARGE_WORKBOOK_CELLS, PreparedChange, SharedCommand, WorkbookIds, decode_entry, encode_command, resolve_command};
use crate::fingerprint::{DocId, fingerprint};
use crate::{Calc, EngineError, Selection, Session};

/// The shared portion of one open document. Personal view and history state are deliberately
/// excluded.
#[derive(Clone, Debug)]
pub struct SharedDocumentSnapshot {
    wb: Arc<gridcraft_model::Workbook>,
    calc: Calc,
    revision: u64,
}

/// A rollback point for the document and its session identities.
#[derive(Clone, Debug)]
pub struct GridSnapshot {
    document: SharedDocumentSnapshot,
    ids: WorkbookIds,
}

#[derive(Clone, Debug)]
struct PersonalView {
    active_sheet: LiveId,
    active_position: usize,
    selections: BTreeMap<LiveId, Selection>,
    exact_selection: Selection,
    exact_sheet_selections: Vec<Selection>,
}

/// GridCraft's document adapter for the shared live-client apply loop.
pub struct GridLiveEditor {
    session: Session,
    ids: WorkbookIds,
    applying_seq: Option<Seq>,
    #[cfg(test)]
    test_command_override: Option<&'static str>,
}

impl GridLiveEditor {
    pub fn new(session: Session) -> Result<Self, super::CoauthorError> {
        let ids = WorkbookIds::from_workbook(&session.doc()?.wb)?;
        Ok(Self {
            session,
            ids,
            applying_seq: None,
            #[cfg(test)]
            test_command_override: None,
        })
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    pub fn ids(&self) -> &WorkbookIds {
        &self.ids
    }

    pub fn prepare(
        &mut self,
        action: u64,
        id: &str,
        params: &Json,
        origin_year: i32,
        allocate: &mut dyn FnMut(u32) -> Result<LiveId, IdError>,
    ) -> Result<PreparedChange, CoauthorError> {
        let entry = encode_command(&self.ids, &self.session, action, id, params, origin_year, allocate)?;
        let payload = serde_json::to_string(&entry).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
        let structural = matches!(
            entry.command,
            SharedCommand::InsertRows { .. }
                | SharedCommand::DeleteRows { .. }
                | SharedCommand::InsertColumns { .. }
                | SharedCommand::DeleteColumns { .. }
                | SharedCommand::InsertSheet { .. }
                | SharedCommand::RenameSheet { .. }
                | SharedCommand::DeleteSheet { .. }
        );
        let large = self.session.doc()?.wb.sheets.iter().fold(0usize, |count, sheet| count.saturating_add(sheet.cells.len())) >= LARGE_WORKBOOK_CELLS;
        let mode = if structural && large { collab_live::client::Mode::WaitForServer } else { collab_live::client::Mode::ShowAtOnce };
        Ok(PreparedChange { payload, mode, action })
    }

    /// The ordered entry currently being applied, retained when that apply crashes so the host
    /// can persist it for the next live client.
    pub fn applying_seq(&self) -> Option<Seq> {
        self.applying_seq
    }

    fn apply_stamp(&mut self, stamp: Stamp) {
        if let Ok(document) = self.session.doc_mut() {
            document.calc.fixed_now = Some(gridcraft_core::date::serial_from_unix_ms(stamp.at_ms as f64));
            document.calc.set_rand_seed(u64::from(stamp.seed));
        }
    }

    pub fn agreement_digest(&self) -> [u8; 32] {
        fingerprint(&self.session, DocId(self.session.active_index())).entered
    }

    fn capture_view(&self) -> Option<PersonalView> {
        let document = self.session.doc().ok()?;
        let active_position = document.wb.active_sheet();
        let active_sheet = self.ids.sheet_id(active_position).ok()?;
        let mut selections = BTreeMap::new();
        for position in 0..document.wb.sheets.len() {
            let Ok(sheet) = self.ids.sheet_id(position) else { continue };
            let selection = if position == active_position {
                document.selection.clone()
            } else {
                document.sheet_selections.get(position).cloned().unwrap_or_default()
            };
            selections.insert(sheet, selection);
        }
        Some(PersonalView {
            active_sheet,
            active_position,
            selections,
            exact_selection: document.selection.clone(),
            exact_sheet_selections: document.sheet_selections.clone(),
        })
    }

    fn restore_view(&mut self, view: &PersonalView) {
        let sheet_count = match self.session.doc() {
            Ok(document) => document.wb.sheets.len(),
            Err(_) => return,
        };
        if sheet_count == 0 {
            return;
        }
        let active_position = self.ids.resolve_sheet(view.active_sheet).unwrap_or_else(|_| view.active_position.min(sheet_count.saturating_sub(1)));
        let mut sheet_selections = Vec::with_capacity(sheet_count);
        for position in 0..sheet_count {
            let selection = self.ids.sheet_id(position).ok().and_then(|sheet| view.selections.get(&sheet).cloned()).unwrap_or_default();
            sheet_selections.push(selection);
        }
        let selection = sheet_selections.get(active_position).cloned().unwrap_or_default();
        if let Ok(document) = self.session.doc_mut() {
            document.wb.set_active_sheet(active_position);
            document.selection = selection;
            document.sheet_selections = sheet_selections;
        }
    }

    fn restore_exact_view(&mut self, view: &PersonalView) {
        if let Ok(document) = self.session.doc_mut() {
            document.wb.set_active_sheet(view.active_position);
            document.selection = view.exact_selection.clone();
            document.sheet_selections = view.exact_sheet_selections.clone();
        }
    }

    fn restore_shared(&mut self, snapshot: &GridSnapshot, view: Option<&PersonalView>, exact_view: bool) {
        if let Ok(document) = self.session.doc_mut() {
            document.wb = snapshot.document.wb.clone();
            document.calc = snapshot.document.calc.clone();
            document.revision = snapshot.document.revision;
            self.ids = snapshot.ids.clone();
        }
        self.session.take_ui_requests();
        if let Some(view) = view {
            if exact_view {
                self.restore_exact_view(view);
            } else {
                self.restore_view(view);
            }
        }
    }

    fn no_effect(reason: &str, read_stamp: bool) -> Outcome {
        Outcome { effect: Effect::NoEffect { reason: reason.to_string() }, read_stamp }
    }

    #[doc(hidden)]
    pub fn engine_error_outcome(error: EngineError, read_stamp: bool) -> Result<Outcome, Crash> {
        match error {
            EngineError::Internal(_, _) => Err(Crash),
            _ => Ok(Self::no_effect("The shared change could not be applied.", read_stamp)),
        }
    }
}

impl LiveEditor for GridLiveEditor {
    type Snapshot = GridSnapshot;

    fn snapshot(&self) -> GridSnapshot {
        let document = self.session.doc().map_or_else(
            |_| SharedDocumentSnapshot { wb: Arc::new(gridcraft_model::Workbook::new()), calc: Calc::new(), revision: 0 },
            |document| SharedDocumentSnapshot { wb: document.wb.clone(), calc: document.calc.clone(), revision: document.revision },
        );
        GridSnapshot { document, ids: self.ids.clone() }
    }

    fn restore(&mut self, snapshot: &GridSnapshot) {
        let view = self.capture_view();
        self.restore_shared(snapshot, view.as_ref(), false);
    }

    fn apply(&mut self, entry: &EntryView<'_>) -> Result<Outcome, Crash> {
        let decoded = match decode_entry(entry.payload) {
            Ok(decoded) => decoded,
            Err(_) => return Ok(Self::no_effect("The shared change is not valid.", false)),
        };
        let resolved = match resolve_command(&self.ids, &self.session, &decoded) {
            Ok(resolved) => resolved,
            Err(_) => return Ok(Self::no_effect("The shared target is no longer available.", false)),
        };
        let view = self.capture_view();
        let before = self.snapshot();
        if let Ok(document) = self.session.doc_mut() {
            document.calc.clear_stamp_read();
        }
        if let Some(stamp) = entry.stamp {
            self.apply_stamp(stamp);
        }
        #[cfg(test)]
        let command_id = self.test_command_override.unwrap_or(resolved.id.as_str());
        #[cfg(not(test))]
        let command_id = resolved.id.as_str();
        let result = self.session.execute(command_id, resolved.params);
        let read_stamp = entry.stamp.is_none() && self.session.doc().is_ok_and(|document| document.calc.stamp_was_read());
        if let Err(error) = result {
            self.restore_shared(&before, view.as_ref(), true);
            return Self::engine_error_outcome(error, read_stamp);
        }
        if let Some(structural) = resolved.structural
            && self.ids.apply_structural(&structural).is_err()
        {
            self.restore_shared(&before, view.as_ref(), true);
            return Ok(Self::no_effect("The shared identities could not be updated.", read_stamp));
        }
        let first_local_apply = entry.own && entry.seq.is_none();
        if !first_local_apply && let Some(view) = view.as_ref() {
            self.restore_view(view);
        }
        Ok(Outcome { effect: Effect::Applied, read_stamp })
    }

    fn note_applying(&mut self, seq: Option<Seq>) {
        // G3 keeps this crash note in memory; the G5 host persists it across process reloads.
        self.applying_seq = seq;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Once};

    use collab_live::client::{Effect, EntryView, LiveEditor};
    use collab_live::ids::LiveId;
    use gridcraft_core::CellRef;
    use gridcraft_model::{Sheet, Workbook};
    use protocol::live::ParticipantNo;
    use serde_json::{Value as Json, json};

    use super::GridLiveEditor;
    use crate::{DocState, EngineError, Result, Session, Trust};

    const FAIL_OTHER: &str = "test.coauthorFailOther";
    const FAIL_INTERNAL: &str = "test.coauthorFailInternal";

    fn mutate_shared_state(session: &mut Session) -> Result<()> {
        session.execute("home.insertSheet", json!({"name":"Changed"}))?;
        let document = session.doc_mut()?;
        document.calc.fixed_now = Some(99_999.0);
        document.calc.set_rand_seed(987_654_321);
        document.revision = document.revision.saturating_add(17);
        Ok(())
    }

    fn fail_other(session: &mut Session, _: &Json) -> Result<Json> {
        mutate_shared_state(session)?;
        Err(EngineError::Other("private failure detail".into()))
    }

    fn fail_internal(session: &mut Session, _: &Json) -> Result<Json> {
        mutate_shared_state(session)?;
        Err(EngineError::Internal("test".into(), "private crash detail".into()))
    }

    fn register_failure_commands() {
        static REGISTER: Once = Once::new();
        REGISTER.call_once(|| {
            crate::cmd::register_test_command(FAIL_OTHER, fail_other);
            crate::cmd::register_test_command(FAIL_INTERNAL, fail_internal);
        });
    }

    fn editor() -> GridLiveEditor {
        let mut workbook = Workbook::new();
        workbook.sheets.clear();
        workbook.sheets.extend([Arc::new(Sheet::new("Data")), Arc::new(Sheet::new("View"))]);
        let mut session = Session::new();
        session.add_document(DocState::new(workbook, None, "Book1".into(), Trust::Local));
        session.execute("sheet.activate", json!({"sheet":"View"})).expect("view sheet activates");
        session.execute("selection.set", json!({"range":"C3"})).expect("selection moves");
        {
            let document = session.doc_mut().expect("document exists");
            document.calc.fixed_now = Some(12_345.0);
            document.calc.set_rand_seed(77);
        }
        GridLiveEditor::new(session).expect("editor builds")
    }

    fn assert_failed_apply_rolls_back(command: &'static str, crash: bool) {
        register_failure_commands();
        let mut editor = editor();
        let mut next = 0;
        let change = editor
            .prepare(1, "home.insertSheet", &json!({"name":"Payload"}), 2026, &mut |count| {
                let first = LiveId(7, next);
                next = next.checked_add(count).ok_or(collab_live::ids::IdError::NoPlace)?;
                Ok(first)
            })
            .expect("change prepares");
        let before_workbook = serde_json::to_value(&*editor.session().doc().expect("document exists").wb).expect("workbook serializes");
        let before_calc = format!("{:?}", editor.session().doc().expect("document exists").calc);
        let before_revision = editor.session().doc().expect("document exists").revision;
        let before_ids = editor.ids().clone();
        let before_view = {
            let document = editor.session().doc().expect("document exists");
            (document.wb.active_sheet(), document.selection.clone(), document.sheet_selections.clone())
        };
        editor.test_command_override = Some(command);
        let entry =
            EntryView { seq: Some(collab_core::ids::Seq(1)), participant: ParticipantNo(2), own: false, stamp: None, payload: &change.payload };

        let result = editor.apply(&entry);

        if crash {
            assert_eq!(result, Err(collab_live::client::Crash));
        } else {
            let outcome = result.expect("ordinary engine errors are public no-effects");
            assert_eq!(outcome.effect, Effect::NoEffect { reason: "The shared change could not be applied.".into() });
            assert!(!outcome.read_stamp);
        }
        let document = editor.session().doc().expect("document exists");
        assert_eq!(serde_json::to_value(&*document.wb).expect("workbook serializes"), before_workbook, "workbook rolls back exactly");
        assert_eq!(format!("{:?}", document.calc), before_calc, "calculation state rolls back exactly");
        assert_eq!(document.revision, before_revision, "revision rolls back exactly");
        assert_eq!(editor.ids(), &before_ids, "identity tables roll back exactly");
        assert_eq!(
            (document.wb.active_sheet(), document.selection.clone(), document.sheet_selections.clone()),
            before_view,
            "personal view rolls back exactly"
        );
        assert_eq!(document.wb.active().expect("active sheet exists").name, "View");
        assert_eq!(document.selection.active, CellRef::new(2, 2));
    }

    #[test]
    fn apply_rolls_back_a_mutating_other_error_with_a_stable_reason() {
        assert_failed_apply_rolls_back(FAIL_OTHER, false);
    }

    #[test]
    fn apply_rolls_back_a_mutating_internal_error_and_maps_it_to_crash() {
        assert_failed_apply_rolls_back(FAIL_INTERNAL, true);
    }
}
