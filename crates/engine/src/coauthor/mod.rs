//! Live co-authoring adapter types shared by the command encoder and apply loop.

mod command;
mod editor;
mod host;
mod ids;
mod presence;
mod undo;

use std::collections::{BTreeMap, VecDeque};

pub use collab_core::ids::Seq;
use collab_live::client::{AgreedSnapshot, Effect, LiveClient, Mode};
/// The live client's types a host names, so it needs no dependency of its own on ShareCraft's crates.
pub use collab_live::client::{ClientError, Event, LiveConfig, LocalError, MAX_ENTRIES_PER_WINDOW, Phase, Stamp, Step};
use collab_live::ids::IdError;
use protocol::live::{LiveBuild, MAX_PAYLOAD_BYTES, MajorReason, RefusedReason};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub use command::{
    IdentityAnchor, IdentityRefKind, IdentityReference, IdentitySheetSel, ResolvedCommand, SharedCommand, SharedEntry, StylePatch,
    allocates_only_own_identities, decode_entry, encode_command, is_shared_command, resolve_command,
};
pub use editor::{CrashNote, GridLiveEditor, GridSnapshot, SharedDocumentSnapshot};
pub use host::{
    LOCAL_COMMANDS, LOCAL_QUERIES, LiveBase, LiveNotice, NotAppliedReason, Route, UNSHARED_REASON, explicit_params, is_local_command, route,
};
pub use ids::{
    CellId, IdentityDelete, IdentityInsert, IdentityPosition, IdentityRange, IdentityRangeKind, IdentityRanges, IdentityRun, IdentitySheetInsert,
    IdentityStructural, MAX_SESSION_DATA_BYTES, SheetIds, TargetGone, WorkbookIds,
};
pub use presence::{MAX_PRESENCE_BYTES, MIN_PRESENCE_INTERVAL_MS, Presence, ShownPresence};
pub use undo::{
    GuardedAxisState, GuardedCellContent, GuardedCellInput, GuardedLineFields, GuardedLineState, GuardedPatch, GuardedStructural, GuardedStyleLeaf,
    GuardedValue, HistoryError, PERSONAL_HISTORY_ACTIONS, PERSONAL_HISTORY_BYTES, PersonalHistory, StyleTarget, UndoAction, UndoReport,
};
use undo::{apply_guarded_patch, build_undo_action, unrecorded_action};

/// The longest command id a refusal notice repeats.
const MAX_NOTICE_COMMAND_CHARS: usize = 64;

/// The room compares entered digests at every non-zero multiple of this many positions (G4).
pub const AGREEMENT_EVERY: u64 = 64;

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

/// Chooses the mode of a personal Undo or Redo: one that reverses a structural change follows
/// the same rule as the structural command itself.
pub fn mode_for_guarded(patch: &GuardedPatch, stored_cells: usize) -> Mode {
    fn structural(patch: &GuardedPatch) -> bool {
        match patch {
            GuardedPatch::Structural(_) => true,
            GuardedPatch::Composite(parts) => parts.iter().any(structural),
            GuardedPatch::Cells(_) | GuardedPatch::Styles(_) | GuardedPatch::LocalRefusal(_) => false,
        }
    }
    if structural(patch) && stored_cells >= LARGE_WORKBOOK_CELLS { Mode::WaitForServer } else { Mode::ShowAtOnce }
}

/// One validated local change, ready to hand to the shared live client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedChange {
    pub payload: String,
    pub mode: Mode,
    pub action: u64,
}

/// One exact agreed checkpoint result ready for the live host to upload.
#[derive(Clone)]
pub struct GridCheckpoint {
    pub bytes: Vec<u8>,
    pub session_data: Vec<u8>,
    pub sha256: [u8; 32],
    pub entered: [u8; 32],
}

impl GridCheckpoint {
    /// The SHA-256 of the identity data exactly as it is uploaded with the checkpoint; the room
    /// compares a verifier's with the writer's upload.
    pub fn session_sha256(&self) -> [u8; 32] {
        Sha256::digest(&self.session_data).into()
    }
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
    #[error("live identity session data is {bytes} bytes; the limit is {max}")]
    OversizedSessionData { bytes: usize, max: usize },
    #[error("personal history refused the change: {0}")]
    HistoryRefusal(String),
    #[error(transparent)]
    Identity(IdError),
}

/// What the host does for one checkpoint ticket.
pub enum TicketAnswer {
    /// Upload the checkpoint under the ticket.
    Write { ticket: String, at: Seq, base: Option<String>, checkpoint: GridCheckpoint },
    /// Report the hashes of the independently written checkpoint: its file bytes, and the
    /// identity data a write at `at` would upload with them.
    Verify { ticket: String, at: Seq, sha256: [u8; 32], session_sha256: [u8; 32] },
    /// The ticket cannot be honoured: send the `checkpoint_failed` frame, and post nothing.
    Failed { send: Option<String> },
}

/// A checkpoint of `state`: its bytes and hash, and the identity data a base needs.
fn write_checkpoint(state: &editor::GridCheckpointState<'_>) -> Result<GridCheckpoint, GridLiveError> {
    let written = write_bytes(state)?;
    let session_data = state.ids.session_data()?;
    Ok(GridCheckpoint { bytes: written.bytes, session_data, sha256: written.sha256, entered: written.entered })
}

/// A checkpoint's file bytes, their SHA-256 and the entered digest, without identity data.
struct WrittenBytes {
    bytes: Vec<u8>,
    sha256: [u8; 32],
    entered: [u8; 32],
}

/// The deterministic file bytes of `state` (the stored view applied, file ids restored), their
/// SHA-256 and the entered digest.
fn write_bytes(state: &editor::GridCheckpointState<'_>) -> Result<WrittenBytes, GridLiveError> {
    let mut workbook = state.workbook.clone();
    state.stored_view.apply(&mut workbook, state.ids);
    state.ids.restore_file_ids(&mut workbook)?;
    let bytes = crate::io::save_bytes(&workbook, "checkpoint.xlsx").map_err(CoauthorError::from)?;
    let sha256 = Sha256::digest(&bytes).into();
    let entered = crate::fingerprint::workbook_fingerprint(&workbook).entered;
    Ok(WrittenBytes { bytes, sha256, entered })
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
    /// Own actions whose personal history a lifecycle event discarded, oldest first, with whether
    /// each was an Undo or Redo. The client re-runs them, so each still settles in order: its
    /// outcome reaches its author (a returning change that did nothing keeps its text), but no
    /// history is recorded for it.
    discarded_history_settlements: VecDeque<(u64, bool)>,
    /// The person's old shown copy, written when the room archives an expired return
    /// (`ArchiveReturn`), for the archive ticket that follows.
    archived: Option<GridCheckpoint>,
    highest_applied_seq: u64,
    last_undo_report: Option<UndoReport>,
    /// The text the person typed for each of their pending cell edits, by action, so a change
    /// that does nothing can give it back.
    pending_texts: BTreeMap<u64, String>,
    notices: Vec<LiveNotice>,
    /// Whether the person was told changes wait. Every join hears `changes_paused` or
    /// `changes_resumed` before its catch-up, so only a change of state is told: a resume after
    /// no pause, or a pause heard again on a rejoin, shows nothing.
    paused_told: bool,
    editing: bool,
    presence_sent: Option<String>,
    presence_sent_at: Option<u64>,
}

/// One settled own action: its number and whether it was a personal Undo or Redo.
struct Settled {
    action: u64,
    history_move: bool,
}

#[derive(Clone, Debug)]
enum PendingHistory {
    Original(Option<UndoAction>),
    Undo,
    Redo,
}

impl GridCoauthor {
    pub fn new(config: LiveConfig, editor: GridLiveEditor) -> Self {
        let build = if cfg!(target_arch = "wasm32") { LiveBuild::Browser } else { LiveBuild::Desktop };
        let mut client = LiveClient::new(config).with_live_capabilities(true, build);
        let outbox = client.connected(0);
        Self {
            client,
            editor,
            next_action: 1,
            outbox,
            history: PersonalHistory::default(),
            pending_history: BTreeMap::new(),
            discarded_history_settlements: VecDeque::new(),
            archived: None,
            highest_applied_seq: 0,
            last_undo_report: None,
            pending_texts: BTreeMap::new(),
            notices: Vec::new(),
            paused_told: false,
            editing: false,
            presence_sent: None,
            presence_sent_at: None,
        }
    }

    /// Starts a live editor in the host's session from the document's stored file (the first
    /// live session) or from a durable checkpoint. The base replaces the session's documents.
    pub fn start(session: crate::Session, base: LiveBase<'_>, config: LiveConfig) -> Result<Self, GridLiveError> {
        let editor = match base {
            LiveBase::File { bytes } => GridLiveEditor::from_file_in(session, bytes)?,
            LiveBase::Checkpoint { bytes, session_data, stamp } => GridLiveEditor::from_checkpoint_in(session, bytes, session_data, stamp)?,
            LiveBase::Empty => GridLiveEditor::blank_in(session)?,
        };
        Ok(Self::new(config, editor))
    }

    /// The frame that tells the room this person is leaving; `None` once the client is closed.
    pub fn leave(&mut self) -> Option<String> {
        self.client.leave()
    }

    /// Ends live editing: the session comes back no longer shared, so the macro debugger and
    /// other personal locks work again. Identities and personal history are dropped.
    pub fn finish(self) -> crate::Session {
        self.editor.into_session()
    }

    /// Whether the person may make shared changes.
    pub fn can_edit(&self) -> bool {
        self.client.can_edit()
    }

    /// Notices for the host to show, oldest first, each once.
    pub fn take_notices(&mut self) -> Vec<LiveNotice> {
        std::mem::take(&mut self.notices)
    }

    /// Whether the person is typing in the active cell; carried by presence, never the text.
    pub fn set_editing(&mut self, editing: bool) {
        self.editing = editing;
    }

    /// The person's presence note when it changed since the last one sent, at most one every
    /// [`MIN_PRESENCE_INTERVAL_MS`]; a change held back by the interval goes on a later call.
    pub fn presence(&mut self, now_ms: u64) -> Option<String> {
        // Before the client is live there is no place to show, and the shown editor may still be
        // the blank one from before the base.
        if self.client.phase() != Phase::Live {
            return None;
        }
        let note = presence::encode(self.editor.session(), self.editor.ids(), self.editing)?;
        if self.presence_sent.as_deref() == Some(note.as_str()) {
            return None;
        }
        // A clock that went back counts as the interval having passed, so presence never freezes.
        if self.presence_sent_at.is_some_and(|sent| now_ms >= sent && now_ms - sent < MIN_PRESENCE_INTERVAL_MS) {
            return None;
        }
        self.presence_sent = Some(note.clone());
        self.presence_sent_at = Some(now_ms);
        Some(note)
    }

    /// Another person's note, resolved against this editor's current identity tables for drawing;
    /// `None` for a malformed note or one whose sheet, row or column is gone.
    pub fn shown_presence(&self, note: &str) -> Option<ShownPresence> {
        presence::decode(self.editor.ids(), note)
    }

    /// Refuses a local change before anything is prepared or shown when the person may only view.
    fn refuse_read_only(&self) -> Result<(), GridLiveError> {
        if self.client.phase() == Phase::Live && !self.client.can_edit() { Err(LocalError::ReadOnly.into()) } else { Ok(()) }
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

    /// The lasting identity of a cell in the shown workbook, for a draft to follow.
    pub fn cell_id(&self, sheet: usize, cell: gridcraft_core::CellRef) -> Option<CellId> {
        self.editor.ids().cell_id(sheet, cell).ok()
    }

    /// Where a cell is now; `None` once its sheet, row or column is gone.
    pub fn find_cell(&self, id: CellId) -> Option<(usize, gridcraft_core::CellRef)> {
        self.editor.ids().resolve_cell(id).ok()
    }

    pub fn last_undo_report(&self) -> Option<&UndoReport> {
        self.last_undo_report.as_ref()
    }

    /// Writes the exact agreed state at `at`, excluding any optimistic work still shown.
    pub fn checkpoint(&self, at: collab_core::ids::Seq) -> Result<GridCheckpoint, GridLiveError> {
        let state = match self.client.agreed_snapshot(&self.editor, at)? {
            AgreedSnapshot::Editor(editor) => editor.checkpoint_state()?,
            AgreedSnapshot::Saved(snapshot) => snapshot.checkpoint_state(),
        };
        write_checkpoint(&state)
    }

    /// The copy the person sees now, waiting changes included, as deterministic checkpoint bytes.
    /// It carries no identity data, and none is built: the room stores it below the current file
    /// with no live checkpoint row, which is what keeps it from ever being loaded as a base (the
    /// web seat loads any base with empty identity data as a plain file).
    fn shown_copy(&self) -> Result<GridCheckpoint, GridLiveError> {
        let state = self.editor.checkpoint_state()?;
        let written = write_bytes(&state)?;
        Ok(GridCheckpoint { bytes: written.bytes, session_data: Vec::new(), sha256: written.sha256, entered: written.entered })
    }

    /// Answers a checkpoint or verify ticket from the exact agreed state at its position; `None`
    /// for any other event. A failure is answered with the `checkpoint_failed` frame only.
    pub fn answer_ticket(&self, event: &Event) -> Option<TicketAnswer> {
        let failed = |ticket| TicketAnswer::Failed { send: serde_json::to_string(&protocol::live::LiveClientMsg::CheckpointFailed { ticket }).ok() };
        if let Event::CheckpointRequested { ticket, at, base, major: Some(MajorReason::ReturningClientArchive) } = event {
            // The room keeps the person's old copy below the current file (C23): answer with
            // the copy written when the return was archived, never the agreed state.
            return Some(match self.archived.clone() {
                Some(checkpoint) => TicketAnswer::Write { ticket: ticket.to_string(), at: *at, base: base.clone(), checkpoint },
                None => failed(*ticket),
            });
        }
        let (ticket, at, write, base) = match event {
            Event::CheckpointRequested { ticket, at, base, .. } => (*ticket, *at, true, base.clone()),
            Event::VerifyRequested { ticket, at, base } => (*ticket, *at, false, base.clone()),
            _ => return None,
        };
        let Ok(checkpoint) = self.checkpoint(at) else {
            return Some(TicketAnswer::Failed { send: serde_json::to_string(&protocol::live::LiveClientMsg::CheckpointFailed { ticket }).ok() });
        };
        let ticket = ticket.to_string();
        Some(if write {
            TicketAnswer::Write { ticket, at, base, checkpoint }
        } else {
            TicketAnswer::Verify { ticket, at, sha256: checkpoint.sha256, session_sha256: checkpoint.session_sha256() }
        })
    }

    /// Where the client is in its session.
    pub fn phase(&self) -> Phase {
        self.client.phase()
    }

    /// How many of the person's changes wait for the server.
    pub fn waiting(&self) -> usize {
        self.client.pending()
    }

    /// Keeps the crash note where it survives the editor stopping: called with the position of
    /// each received entry before it is applied, and with `None` once it is.
    pub fn set_crash_note(&mut self, note: CrashNote) {
        self.editor.set_crash_note(note);
    }

    /// Resets the editor to the base the client asked for ([`Event::LoadBase`]); call
    /// [`Self::base_ready`] next. Work is done in a new editor, so a base that cannot be opened
    /// leaves the current one as it was.
    pub fn load_base(&mut self, base: LiveBase<'_>) -> Result<(), GridLiveError> {
        let mut editor = match base {
            LiveBase::Empty => GridLiveEditor::blank()?,
            LiveBase::File { bytes } => GridLiveEditor::from_file_in(crate::Session::new(), bytes)?,
            LiveBase::Checkpoint { bytes, session_data, stamp } => GridLiveEditor::from_checkpoint(bytes, session_data, stamp)?,
        };
        editor.take_crash_note_from(&mut self.editor);
        self.editor = editor;
        Ok(())
    }

    /// Replaces the shown editor with a fully recalculated durable checkpoint. Work is done in a
    /// temporary editor, so malformed bytes or identity data leave the current editor untouched.
    pub fn load_checkpoint(&mut self, bytes: &[u8], session_data: &[u8], stamp: collab_live::client::Stamp) -> Result<(), GridLiveError> {
        let mut editor = GridLiveEditor::from_checkpoint(bytes, session_data, stamp)?;
        editor.take_crash_note_from(&mut self.editor);
        self.editor = editor;
        Ok(())
    }

    /// The existing live connection dropped. The in-memory client and its personal history are
    /// kept so a server-confirmed kept place can resume them.
    pub fn disconnected(&mut self, now_ms: u64) {
        self.client.disconnected(now_ms);
        self.forget_presence();
    }

    /// The same in-memory client opened a replacement connection.
    pub fn connected(&mut self, now_ms: u64) -> Vec<String> {
        self.forget_presence();
        self.client.connected(now_ms)
    }

    /// The room drops a person's note with their connection or place, so the next note goes
    /// even when it is unchanged.
    fn forget_presence(&mut self) {
        self.presence_sent = None;
        self.presence_sent_at = None;
    }

    pub fn submit(&mut self, id: &str, params: Json, origin_year: i32, now_ms: u64) -> Result<Step, GridLiveError> {
        if !is_shared_command(id) {
            self.notices.push(LiveNotice::Unshared { command: id.chars().take(MAX_NOTICE_COMMAND_CHARS).collect() });
            return Err(CoauthorError::UnsupportedCommand(id.to_string()).into());
        }
        self.refuse_read_only()?;
        let next_action = self.next_action.checked_add(1).ok_or(GridLiveError::ActionExhausted)?;
        let action = self.next_action;
        let client = &mut self.client;
        let prepared = self.editor.prepare(action, id, &params, origin_year, &mut |count| client.new_ids(count))?;
        let step = self.client.local(&mut self.editor, prepared.payload, prepared.mode, now_ms)?;
        let history = self.editor.take_history_update(action).unwrap_or(None);
        self.pending_history.insert(action, PendingHistory::Original(history));
        if id == "cell.set"
            && let Some(typed) = params.get("input").or_else(|| params.get("value")).filter(|value| !value.is_null())
        {
            self.pending_texts.insert(action, crate::cmd::edit::json_to_input(typed));
        }
        self.next_action = next_action;
        Ok(step)
    }

    pub fn undo(&mut self, now_ms: u64) -> Result<Step, GridLiveError> {
        self.refuse_read_only()?;
        let patch = self.history.prepare_undo().map_err(|error| self.history_refused(error.to_string()))?;
        self.submit_history_patch(patch, PendingHistory::Undo, now_ms)
    }

    pub fn redo(&mut self, now_ms: u64) -> Result<Step, GridLiveError> {
        self.refuse_read_only()?;
        let patch = self.history.prepare_redo().map_err(|error| self.history_refused(error.to_string()))?;
        self.submit_history_patch(patch, PendingHistory::Redo, now_ms)
    }

    fn history_refused(&mut self, reason: String) -> CoauthorError {
        self.notices.push(LiveNotice::UndoRefused { reason: reason.clone() });
        CoauthorError::HistoryRefusal(reason)
    }

    fn submit_history_patch(&mut self, patch: GuardedPatch, pending: PendingHistory, now_ms: u64) -> Result<Step, GridLiveError> {
        let next_action = self.next_action.checked_add(1).ok_or(GridLiveError::ActionExhausted)?;
        let action = self.next_action;
        let mode = mode_for_guarded(&patch, self.editor.stored_cells());
        let entry = command::guarded_entry(action, patch);
        let payload = serde_json::to_string(&entry).map_err(|error| CoauthorError::MalformedPayload(error.to_string()))?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            self.history.refused(action);
            return Err(CoauthorError::HistoryRefusal("This change is too large to undo while sharing".into()).into());
        }
        let step = match self.client.local(&mut self.editor, payload, mode, now_ms) {
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
        let agreed_before = self.client.agreed_seq();
        let result = self.client.receive(&mut self.editor, frame, now_ms);
        // A first WaitForServer application may intentionally change the view at acknowledgement;
        // every replay and remote application must keep the view captured around the whole receive.
        if self.editor.local_action_high_water() == local_action_high_water
            && let Some(view) = view.as_ref()
        {
            self.editor.restore_view(view);
        }
        let mut step = result?;
        self.reconcile_history(&step);
        self.report_agreement(agreed_before, &mut step);
        Ok(step)
    }

    pub fn base_ready(&mut self, now_ms: u64) -> Result<Step, GridLiveError> {
        let view = self.editor.capture_view();
        let agreed_before = self.client.agreed_seq();
        let result = self.client.base_ready(&mut self.editor, now_ms);
        if let Some(view) = view.as_ref() {
            self.editor.restore_view(view);
        }
        let mut step = result?;
        self.reconcile_history(&step);
        self.report_agreement(agreed_before, &mut step);
        Ok(step)
    }

    /// Reports the entered digest when the agreed state has just reached a non-zero multiple of
    /// [`AGREEMENT_EVERY`]: the room compares editors' reports there and resyncs a client that
    /// disagrees. Only an editing, live client reports, and only when the agreed state is exactly
    /// at that position; a catch-up that passes over one in a single step does not report it.
    fn report_agreement(&mut self, before: Seq, step: &mut Step) {
        let at = self.client.agreed_seq();
        if at == before || at.0 == 0 || !at.0.is_multiple_of(AGREEMENT_EVERY) || !self.client.can_edit() || self.client.phase() != Phase::Live {
            return;
        }
        let entered = match self.client.agreed_snapshot(&self.editor, at) {
            Ok(AgreedSnapshot::Editor(editor)) => {
                editor.checkpoint_state().map(|state| crate::fingerprint::workbook_fingerprint(state.workbook).entered)
            }
            Ok(AgreedSnapshot::Saved(snapshot)) => Ok(crate::fingerprint::workbook_fingerprint(snapshot.checkpoint_state().workbook).entered),
            Err(_) => return,
        };
        if let Ok(entered) = entered
            && let Ok(frame) = self.client.fingerprint(at, entered)
        {
            step.send.push(frame);
        }
    }

    pub fn tick(&mut self, now_ms: u64) -> Vec<String> {
        let mut send = std::mem::take(&mut self.outbox);
        send.extend(self.client.tick(now_ms));
        send
    }

    fn reconcile_history(&mut self, step: &Step) {
        if step.events.iter().any(|event| matches!(event, Event::ArchiveReturn { .. })) {
            // The room archives the person's old copy and the client drops its waiting changes:
            // no acknowledgement will come for them. Keep the copy for the archive ticket.
            match self.shown_copy() {
                Ok(copy) => self.archived = Some(copy),
                Err(_) => self.archived = None,
            }
            self.discarded_history_settlements.clear();
            self.pending_history.clear();
            self.pending_texts.clear();
        }
        if step.events.iter().any(|event| matches!(event, Event::LoadBase { .. } | Event::Returned { .. })) {
            self.history.clear();
            // LiveClient intentionally keeps and re-runs its queued entries across these
            // lifecycle events. Keep one inert FIFO slot per discarded history move so an old
            // acknowledgement cannot settle a newer action submitted after the reconnect; the
            // slot keeps its action, so the author still hears what the change did, with the
            // text they typed (a returning change on a deleted target, C22).
            let discarded: Vec<(u64, bool)> = self
                .pending_history
                .iter()
                .map(|(action, pending)| (*action, matches!(pending, PendingHistory::Undo | PendingHistory::Redo)))
                .collect();
            self.discarded_history_settlements.extend(discarded);
            self.pending_history.clear();
            self.last_undo_report = None;
            self.forget_presence();
            self.editor.clear_history_results();
            self.editor.clear_own_no_effects();
        }
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
        let mut returned_not_applied: Option<usize> = None;
        for event in &step.events {
            match event {
                Event::Applied { seq, own, effect } => {
                    let is_new = seq.0 > self.highest_applied_seq;
                    self.highest_applied_seq = self.highest_applied_seq.max(seq.0);
                    if *own && is_new {
                        let settled = self.settle_next_history(true);
                        self.notice_own_outcome(*seq, effect, settled);
                        if matches!(effect, Effect::NoEffect { .. })
                            && let Some(count) = returned_not_applied.as_mut()
                        {
                            *count = count.saturating_add(1);
                        }
                    }
                }
                Event::Refused { reason, .. } => {
                    if let Some(settled) = self.settle_next_history(false) {
                        self.forget_own(settled.action);
                    }
                    self.notices.push(LiveNotice::ChangeRefused { reason: *reason });
                }
                Event::DroppedPending { count } => {
                    for _ in 0..*count {
                        if let Some(settled) = self.settle_next_history(false) {
                            self.forget_own(settled.action);
                        }
                    }
                    if *count > 0 {
                        self.notices.push(LiveNotice::ChangeRefused { reason: RefusedReason::ReadOnly });
                    }
                }
                Event::Returned { .. } => returned_not_applied = Some(0),
                Event::ArchiveReturn { .. } => self.notices.push(LiveNotice::ArchivedReturn),
                Event::AccessChanged { can_edit } => self.notices.push(LiveNotice::ReadOnly { read_only: !can_edit }),
                Event::ChangesPaused { entries, bytes } => {
                    if !std::mem::replace(&mut self.paused_told, true) {
                        self.notices.push(LiveNotice::Paused { entries: *entries, bytes: *bytes });
                    }
                }
                Event::ChangesResumed => {
                    if std::mem::replace(&mut self.paused_told, false) {
                        self.notices.push(LiveNotice::Resumed);
                    }
                }
                Event::CannotOpen { .. } => self.notices.push(LiveNotice::CannotOpen),
                Event::Incompatible { .. } => self.notices.push(LiveNotice::Incompatible),
                Event::Removed { .. } => self.notices.push(LiveNotice::Removed),
                _ => {}
            }
        }
        if let Some(not_applied) = returned_not_applied {
            self.notices.push(LiveNotice::Returned { not_applied });
        }
        // Rollback/re-run and catch-up can produce results for actions whose personal history
        // was discarded at a lifecycle boundary. Every active action was drained above.
        self.editor.clear_history_results();
    }

    /// Tells the person what their settled change did: a cell edit that did nothing keeps its
    /// text, and Undo or Redo reports the cells it left.
    fn notice_own_outcome(&mut self, seq: collab_core::ids::Seq, effect: &Effect, settled: Option<Settled>) {
        let Some(settled) = settled else { return };
        let reason = self.editor.take_own_no_effect(settled.action);
        let kept_text = self.pending_texts.remove(&settled.action);
        if settled.history_move {
            if let Some(cells) = self.last_undo_report.as_ref().map(|report| report.left).filter(|left| *left > 0) {
                self.notices.push(LiveNotice::UndoLeft { cells });
            }
            return;
        }
        if matches!(effect, Effect::NoEffect { .. }) {
            self.notices.push(LiveNotice::NotApplied { seq, kept_text, reason: reason.unwrap_or(NotAppliedReason::Invalid) });
        }
    }

    fn forget_own(&mut self, action: u64) {
        self.pending_texts.remove(&action);
        self.editor.take_own_no_effect(action);
    }

    fn settle_next_history(&mut self, accepted: bool) -> Option<Settled> {
        if let Some((action, history_move)) = self.discarded_history_settlements.pop_front() {
            // Its history went with the lifecycle event; only its outcome is reported.
            if history_move {
                self.last_undo_report = None;
            }
            return Some(Settled { action, history_move });
        }
        self.settle_first_history(accepted)
    }

    fn settle_first_history(&mut self, accepted: bool) -> Option<Settled> {
        let action = self.pending_history.keys().next().copied()?;
        let pending = self.pending_history.remove(&action)?;
        let history_move = matches!(pending, PendingHistory::Undo | PendingHistory::Redo);
        match (pending, accepted) {
            (PendingHistory::Original(Some(history)), true) => self.history.record(history),
            (PendingHistory::Undo | PendingHistory::Redo, true) => self.history.accepted(action),
            (PendingHistory::Undo | PendingHistory::Redo, false) => {
                self.history.refused(action);
                self.last_undo_report = None;
            }
            (PendingHistory::Original(_), false | true) => {}
        }
        Some(Settled { action, history_move })
    }
}
