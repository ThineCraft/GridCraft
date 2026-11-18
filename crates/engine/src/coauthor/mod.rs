//! Live co-authoring adapter types shared by the command encoder and apply loop.

mod command;
mod editor;
mod ids;
mod undo;

use std::collections::BTreeMap;

use collab_live::client::{ClientError, Event, LiveClient, LiveConfig, LocalError, Mode, Step};
use collab_live::ids::IdError;
use protocol::live::MAX_PAYLOAD_BYTES;
use serde_json::Value as Json;
use thiserror::Error;

pub use command::{
    IdentityAnchor, IdentityRefKind, IdentityReference, IdentitySheetSel, ResolvedCommand, SharedCommand, SharedEntry, decode_entry, encode_command,
    is_shared_command, resolve_command,
};
pub use editor::{GridLiveEditor, GridSnapshot, SharedDocumentSnapshot};
pub use ids::{
    CellId, IdentityDelete, IdentityInsert, IdentityPosition, IdentityRange, IdentityRangeKind, IdentityRanges, IdentitySheetInsert,
    IdentityStructural, SheetIds, TargetGone, WorkbookIds,
};
pub use undo::{
    GuardedCellContent, GuardedPatch, GuardedStructural, GuardedStyleLeaf, GuardedValue, HistoryError, PERSONAL_HISTORY_ACTIONS,
    PERSONAL_HISTORY_BYTES, PersonalHistory, StyleTarget, UndoAction, UndoReport,
};
use undo::{apply_guarded_patch, build_undo_action};

/// The format name negotiated with a live room.
pub const GRIDCRAFT_LIVE_FORMAT: &str = "gridcraft/1";

/// Structural changes at or above this size wait for the server before they are shown.
pub const LARGE_WORKBOOK_CELLS: usize = 100_000;

/// Chooses whether a shared command is shown before the server orders it.
pub fn mode_for(command: &SharedCommand, stored_cells: usize) -> Mode {
    let structural = matches!(
        command,
        SharedCommand::InsertRows { .. }
            | SharedCommand::DeleteRows { .. }
            | SharedCommand::InsertColumns { .. }
            | SharedCommand::DeleteColumns { .. }
            | SharedCommand::InsertSheet { .. }
            | SharedCommand::RenameSheet { .. }
            | SharedCommand::DeleteSheet { .. }
    );
    if structural && stored_cells >= LARGE_WORKBOOK_CELLS { Mode::WaitForServer } else { Mode::ShowAtOnce }
}

/// One validated local change, ready to hand to the shared live client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedChange {
    pub payload: String,
    pub mode: Mode,
    pub action: u64,
}

/// A live entry that cannot be decoded, targeted or applied safely.
#[derive(Debug, Error)]
pub enum CoauthorError {
    #[error("malformed GridCraft live entry: {0}")]
    MalformedPayload(String),
    #[error("unsupported GridCraft live entry version {0}")]
    UnsupportedVersion(u64),
    #[error("command `{0}` is not shared")]
    UnsupportedCommand(String),
    #[error("command `{command}` is missing required `{target}`")]
    MissingTarget { command: String, target: String },
    #[error("unknown live identity: {0}")]
    UnknownIdentity(String),
    #[error("live target was deleted: {0}")]
    DeletedTarget(String),
    #[error(transparent)]
    Engine(#[from] crate::EngineError),
    #[error("live entry is {bytes} bytes; the limit is {max}")]
    OversizedPayload { bytes: usize, max: usize },
    #[error("personal history refused the change: {0}")]
    HistoryRefusal(String),
    #[error(transparent)]
    Identity(IdError),
}

/// An error from preparing a GridCraft entry or advancing the shared live client.
#[derive(Debug, Error)]
pub enum GridLiveError {
    #[error(transparent)]
    Coauthor(#[from] CoauthorError),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Local(#[from] LocalError),
    #[error("shared action numbers are exhausted")]
    ActionExhausted,
}

/// GridCraft's host-facing composition of one editor and G2's ordered live client.
pub struct GridCoauthor {
    client: LiveClient<GridLiveEditor>,
    editor: GridLiveEditor,
    next_action: u64,
    outbox: Vec<String>,
    history: PersonalHistory,
    pending_history: BTreeMap<u64, PendingHistory>,
    last_undo_report: Option<UndoReport>,
}

#[derive(Clone, Debug)]
enum PendingHistory {
    Original(Option<UndoAction>),
    Undo,
    Redo,
}

impl GridCoauthor {
    pub fn new(config: LiveConfig, editor: GridLiveEditor) -> Self {
        let mut client = LiveClient::new(config);
        let outbox = client.connected(0);
        Self { client, editor, next_action: 1, outbox, history: PersonalHistory::default(), pending_history: BTreeMap::new(), last_undo_report: None }
    }

    pub fn editor(&self) -> &GridLiveEditor {
        &self.editor
    }

    /// Mutable access for personal-only state such as the active sheet and selection.
    /// Shared document changes must go through [`Self::submit`].
    pub fn editor_mut(&mut self) -> &mut GridLiveEditor {
        &mut self.editor
    }

    pub fn history(&self) -> &PersonalHistory {
        &self.history
    }

    pub fn last_undo_report(&self) -> Option<&UndoReport> {
        self.last_undo_report.as_ref()
    }

    pub fn submit(&mut self, id: &str, params: Json, origin_year: i32, now_ms: u64) -> Result<Step, GridLiveError> {
        let next_action = self.next_action.checked_add(1).ok_or(GridLiveError::ActionExhausted)?;
        let action = self.next_action;
        let client = &mut self.client;
        let prepared = self.editor.prepare(action, id, &params, origin_year, &mut |count| client.new_ids(count))?;
        let step = self.client.local(&mut self.editor, prepared.payload, prepared.mode, now_ms)?;
        let history = self.editor.take_history_update(action).unwrap_or(None);
        self.pending_history.insert(action, PendingHistory::Original(history));
        self.next_action = next_action;
        Ok(step)
    }

    pub fn undo(&mut self, now_ms: u64) -> Result<Step, GridLiveError> {
        let patch = self.history.prepare_undo().map_err(|error| CoauthorError::HistoryRefusal(error.to_string()))?;
        self.submit_history_patch(patch, PendingHistory::Undo, now_ms)
    }

    pub fn redo(&mut self, now_ms: u64) -> Result<Step, GridLiveError> {
        let patch = self.history.prepare_redo().map_err(|error| CoauthorError::HistoryRefusal(error.to_string()))?;
        self.submit_history_patch(patch, PendingHistory::Redo, now_ms)
    }

    fn submit_history_patch(&mut self, patch: GuardedPatch, pending: PendingHistory, now_ms: u64) -> Result<Step, GridLiveError> {
        let next_action = self.next_action.checked_add(1).ok_or(GridLiveError::ActionExhausted)?;
        let action = self.next_action;
        let entry = command::guarded_entry(action, patch);
        let payload = serde_json::to_string(&entry).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            self.history.refused(action);
            return Err(CoauthorError::HistoryRefusal("This change is too large to undo while sharing".into()).into());
        }
        let step = match self.client.local(&mut self.editor, payload, Mode::ShowAtOnce, now_ms) {
            Ok(step) => step,
            Err(error) => {
                self.history.refused(action);
                return Err(error.into());
            }
        };
        self.pending_history.insert(action, pending);
        if let Some(report) = self.editor.take_undo_report(action) {
            self.last_undo_report = Some(report);
        }
        self.next_action = next_action;
        Ok(step)
    }

    pub fn receive(&mut self, frame: &str, now_ms: u64) -> Result<Step, GridLiveError> {
        let view = self.editor.capture_view();
        let local_action_high_water = self.editor.local_action_high_water();
        let result = self.client.receive(&mut self.editor, frame, now_ms);
        // A first WaitForServer application may intentionally change the view at acknowledgement;
        // every replay and remote application must keep the view captured around the whole receive.
        if self.editor.local_action_high_water() == local_action_high_water
            && let Some(view) = view.as_ref()
        {
            self.editor.restore_view(view);
        }
        let step = result?;
        self.reconcile_history(&step);
        Ok(step)
    }

    pub fn base_ready(&mut self, now_ms: u64) -> Result<Step, GridLiveError> {
        let view = self.editor.capture_view();
        let result = self.client.base_ready(&mut self.editor, now_ms);
        if let Some(view) = view.as_ref() {
            self.editor.restore_view(view);
        }
        let step = result?;
        self.reconcile_history(&step);
        Ok(step)
    }

    pub fn tick(&mut self, now_ms: u64) -> Vec<String> {
        let mut send = std::mem::take(&mut self.outbox);
        send.extend(self.client.tick(now_ms));
        send
    }

    fn reconcile_history(&mut self, step: &Step) {
        let actions: Vec<u64> = self.pending_history.keys().copied().collect();
        for action in actions {
            if let Some(update) = self.editor.take_history_update(action)
                && let Some(PendingHistory::Original(stored)) = self.pending_history.get_mut(&action)
            {
                *stored = update;
            }
            if let Some(report) = self.editor.take_undo_report(action) {
                self.last_undo_report = Some(report);
            }
        }
        for event in &step.events {
            match event {
                Event::Applied { own: true, .. } => self.settle_first_history(true),
                Event::Refused { .. } => self.settle_first_history(false),
                Event::DroppedPending { count } => {
                    for _ in 0..*count {
                        self.settle_first_history(false);
                    }
                }
                _ => {}
            }
        }
    }

    fn settle_first_history(&mut self, accepted: bool) {
        let Some(action) = self.pending_history.keys().next().copied() else { return };
        let Some(pending) = self.pending_history.remove(&action) else { return };
        match (pending, accepted) {
            (PendingHistory::Original(Some(history)), true) => self.history.record(history),
            (PendingHistory::Undo | PendingHistory::Redo, true) => self.history.accepted(action),
            (PendingHistory::Undo | PendingHistory::Redo, false) => {
                self.history.refused(action);
                self.last_undo_report = None;
            }
            (PendingHistory::Original(_), false | true) => {}
        }
    }
}
