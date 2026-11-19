use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Once;

use collab_core::ids::Seq;
use collab_live::client::{Crash, Effect, EntryView, LiveEditor, Outcome, Stamp};
use collab_live::ids::{IdError, LiveId};
use serde_json::Value as Json;

use super::{
    CoauthorError, GuardedPatch, GuardedStructural, PreparedChange, SharedCommand, TargetGone, UndoAction, UndoReport, WorkbookIds,
    allocates_only_own_identities, apply_guarded_patch, build_undo_action, decode_entry, encode_command, mode_for, resolve_command,
    unrecorded_action,
};
use crate::fingerprint::{DocId, fingerprint};
use crate::{Calc, EngineError, Selection, Session};

const GUARDED_APPLY_COMMAND: &str = "internal.coauthorGuardedPatch";

#[derive(Clone)]
struct GuardedApplyContext {
    ids: WorkbookIds,
    patch: super::GuardedPatch,
    participant: u32,
}

thread_local! {
    static GUARDED_APPLY: RefCell<Option<GuardedApplyContext>> = const { RefCell::new(None) };
}

fn run_guarded_apply(session: &mut Session, _: &Json) -> crate::Result<Json> {
    let context = GUARDED_APPLY.with(|slot| slot.borrow_mut().take());
    let mut context = context.ok_or_else(|| EngineError::Other("the co-author guarded command is internal".into()))?;
    let revision = session.doc()?.revision;
    let report = apply_guarded_patch(session, &mut context.ids, &context.patch, context.participant)?;
    if report.restored > 0 {
        let document = session.doc_mut()?;
        if document.revision == revision {
            document.revision = document.revision.saturating_add(1);
        }
    }
    GUARDED_APPLY.with(|slot| {
        *slot.borrow_mut() = Some(context);
    });
    serde_json::to_value(report).map_err(|error| EngineError::Other(error.to_string()))
}

fn execute_guarded_apply(
    session: &mut Session,
    ids: &mut WorkbookIds,
    patch: &super::GuardedPatch,
    participant: u32,
) -> Result<UndoReport, EngineError> {
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| crate::cmd::register_test_command(GUARDED_APPLY_COMMAND, run_guarded_apply));
    let occupied = GUARDED_APPLY.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_some() {
            true
        } else {
            *slot = Some(GuardedApplyContext { ids: ids.clone(), patch: patch.clone(), participant });
            false
        }
    });
    if occupied {
        return Err(EngineError::Other("a co-author guarded command is already running".into()));
    }
    let journal_paused = session.journal_paused;
    let recorder = session.recorder.take();
    session.journal_paused = session.journal_paused.saturating_add(1);
    let result = session.execute(GUARDED_APPLY_COMMAND, Json::Null);
    session.journal_paused = journal_paused;
    session.recorder = recorder;
    let context = GUARDED_APPLY.with(|slot| slot.borrow_mut().take());
    let value = result?;
    let context = context.ok_or_else(|| EngineError::Other("the co-author guarded command did not return its identity state".into()))?;
    *ids = context.ids;
    serde_json::from_value(value).map_err(|error| EngineError::Other(error.to_string()))
}

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
pub(super) struct PersonalView {
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
    local_action_high_water: u64,
    history_updates: BTreeMap<u64, Option<UndoAction>>,
    undo_reports: BTreeMap<u64, UndoReport>,
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
            local_action_high_water: 0,
            history_updates: BTreeMap::new(),
            undo_reports: BTreeMap::new(),
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
        let stored_cells = self.session.doc()?.wb.sheets.iter().fold(0usize, |count, sheet| count.saturating_add(sheet.cells.len()));
        let mode = mode_for(&entry.command, stored_cells);
        Ok(PreparedChange { payload, mode, action })
    }

    /// The ordered entry currently being applied, retained when that apply crashes so the host
    /// can persist it for the next live client.
    pub fn applying_seq(&self) -> Option<Seq> {
        self.applying_seq
    }

    pub(super) fn local_action_high_water(&self) -> u64 {
        self.local_action_high_water
    }

    pub(super) fn take_history_update(&mut self, action: u64) -> Option<Option<UndoAction>> {
        self.history_updates.remove(&action)
    }

    pub(super) fn take_undo_report(&mut self, action: u64) -> Option<UndoReport> {
        self.undo_reports.remove(&action)
    }

    pub(super) fn clear_history_results(&mut self) {
        self.history_updates.clear();
        self.undo_reports.clear();
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

    pub(super) fn capture_view(&self) -> Option<PersonalView> {
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

    pub(super) fn restore_view(&mut self, view: &PersonalView) {
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

    fn target_no_effect(command: &SharedCommand, error: &TargetGone) -> Outcome {
        let action = serde_json::to_value(command)
            .ok()
            .and_then(|value| value.get("id").and_then(Json::as_str).map(str::to_owned))
            .unwrap_or_else(|| "shared change".to_string());
        let kind = match error {
            TargetGone::DeletedSheet(_) | TargetGone::UnknownSheet(_) => "sheet",
            TargetGone::DeletedRow(_) | TargetGone::UnknownRow(_) => "row",
            TargetGone::DeletedColumn(_) | TargetGone::UnknownColumn(_) => "column",
            TargetGone::UnrepresentableReference => "formula reference",
            TargetGone::Malformed(_) => "target",
        };
        let state = match error {
            TargetGone::DeletedSheet(_) | TargetGone::DeletedRow(_) | TargetGone::DeletedColumn(_) => "was deleted",
            _ => "is not available",
        };
        Self::no_effect(&format!("The `{action}` {kind} target {state}."), false)
    }

    fn guarded_target_kind(patch: &GuardedPatch) -> &'static str {
        match patch {
            GuardedPatch::Cells(_) => "cell content",
            GuardedPatch::Styles(_) => "cell formatting",
            GuardedPatch::Composite(parts) => parts.first().map_or("guarded fields", Self::guarded_target_kind),
            GuardedPatch::LocalRefusal(_) => "sheet",
            GuardedPatch::Structural(structural) => match structural.as_ref() {
                GuardedStructural::InsertRows { .. } | GuardedStructural::DeleteRows { .. } => "rows",
                GuardedStructural::InsertColumns { .. } | GuardedStructural::DeleteColumns { .. } => "columns",
                GuardedStructural::RenameSheet { .. } => "sheet",
            },
        }
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
        // An entry may allocate only its sender's identities; the check reads nothing but the
        // entry, so every client refuses the same entries.
        if !allocates_only_own_identities(&decoded.command, entry.participant.0) {
            return Ok(Self::no_effect("The shared change is not valid.", false));
        }
        if let SharedCommand::Guarded { patch } = &decoded.command {
            let view = self.capture_view();
            let before = self.snapshot();
            let report = match execute_guarded_apply(&mut self.session, &mut self.ids, patch, entry.participant.0) {
                Ok(report) => report,
                Err(error) => {
                    self.restore_shared(&before, view.as_ref(), true);
                    return Self::engine_error_outcome(error, false);
                }
            };
            if entry.own {
                self.undo_reports.insert(decoded.action, report.clone());
            }
            let first_local_apply = entry.own && decoded.action > self.local_action_high_water;
            if first_local_apply {
                self.local_action_high_water = decoded.action;
            }
            if !first_local_apply && let Some(view) = view.as_ref() {
                self.restore_view(view);
            }
            let effect = if report.restored == 0 && report.left > 0 {
                Effect::NoEffect {
                    reason: format!("Undo or Redo left the {} unchanged because it changed after this action.", Self::guarded_target_kind(patch)),
                }
            } else {
                Effect::Applied
            };
            return Ok(Outcome { effect, read_stamp: false });
        }
        if entry.own {
            // Replays replace the first optimistic inverse. `None` is meaningful: the same
            // pending action can become a no-effect after an earlier structural entry lands.
            self.history_updates.insert(decoded.action, None);
        }
        let resolved = match resolve_command(&self.ids, &self.session, &decoded) {
            Ok(resolved) => resolved,
            Err(error) => return Ok(Self::target_no_effect(&decoded.command, &error)),
        };
        let view = self.capture_view();
        let before = self.snapshot();
        if let Ok(document) = self.session.doc_mut() {
            document.calc.clear_stamp_read();
        }
        let prior_stamp = entry.stamp.and_then(|_| self.session.doc().ok().map(|document| document.calc.stamp_state()));
        if let Some(stamp) = entry.stamp {
            self.apply_stamp(stamp);
        }
        #[cfg(test)]
        let command_id = self.test_command_override.unwrap_or(resolved.id.as_str());
        #[cfg(not(test))]
        let command_id = resolved.id.as_str();
        let result = self.session.execute(command_id, resolved.params);
        let stamp_was_read = self.session.doc().is_ok_and(|document| document.calc.stamp_was_read());
        if let Err(error) = result {
            self.restore_shared(&before, view.as_ref(), true);
            return Self::engine_error_outcome(error, entry.stamp.is_none() && stamp_was_read);
        }
        if let Some(structural) = resolved.structural
            && self.ids.apply_structural_owned(&structural, entry.participant.0).is_err()
        {
            self.restore_shared(&before, view.as_ref(), true);
            return Ok(Self::no_effect("The shared identities could not be updated.", entry.stamp.is_none() && stamp_was_read));
        }
        if !stamp_was_read
            && let Some(prior_stamp) = prior_stamp
            && let Ok(document) = self.session.doc_mut()
        {
            document.calc.restore_stamp_state(prior_stamp);
        }
        let read_stamp = entry.stamp.is_none() && stamp_was_read;
        // G2 presents both the first optimistic application and later pending replays as
        // `own` entries without a sequence number. Action numbers increase for this person, so
        // only a new high-water mark is the first application; replay must preserve the view.
        // This also recognizes the first application of a WaitForServer action at acknowledgement.
        let first_local_apply = entry.own && decoded.action > self.local_action_high_water;
        if first_local_apply {
            self.local_action_high_water = decoded.action;
        }
        if !first_local_apply && let Some(view) = view.as_ref() {
            self.restore_view(view);
        }
        if entry.own
            && let Ok(document) = self.session.doc()
        {
            match build_undo_action(decoded.action, &before.ids, &self.ids, &before.document.wb, &document.wb, &decoded.command) {
                Ok(Some(action)) => {
                    self.history_updates.insert(decoded.action, Some(action));
                }
                Ok(None) => {}
                // A reverse that cannot be built is recorded as a refusal, so Undo never
                // silently reverses an older action in its place.
                Err(_) => {
                    self.history_updates.insert(decoded.action, Some(unrecorded_action(decoded.action, &decoded.command)));
                }
            }
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
                let first = LiveId(2, next);
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
