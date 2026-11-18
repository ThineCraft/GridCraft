//! Live co-authoring adapter types shared by the command encoder and apply loop.

mod command;
mod editor;
mod ids;

use collab_live::client::{ClientError, LiveClient, LiveConfig, LocalError, Mode, Step};
use collab_live::ids::IdError;
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
}

impl GridCoauthor {
    pub fn new(config: LiveConfig, editor: GridLiveEditor) -> Self {
        let mut client = LiveClient::new(config);
        let outbox = client.connected(0);
        Self { client, editor, next_action: 1, outbox }
    }

    pub fn editor(&self) -> &GridLiveEditor {
        &self.editor
    }

    /// Mutable access for personal-only state such as the active sheet and selection.
    /// Shared document changes must go through [`Self::submit`].
    pub fn editor_mut(&mut self) -> &mut GridLiveEditor {
        &mut self.editor
    }

    pub fn submit(&mut self, id: &str, params: Json, origin_year: i32, now_ms: u64) -> Result<Step, GridLiveError> {
        let next_action = self.next_action.checked_add(1).ok_or(GridLiveError::ActionExhausted)?;
        let action = self.next_action;
        let client = &mut self.client;
        let prepared = self.editor.prepare(action, id, &params, origin_year, &mut |count| client.new_ids(count))?;
        let step = self.client.local(&mut self.editor, prepared.payload, prepared.mode, now_ms)?;
        self.next_action = next_action;
        Ok(step)
    }

    pub fn receive(&mut self, frame: &str, now_ms: u64) -> Result<Step, GridLiveError> {
        let view = self.editor.capture_view();
        let result = self.client.receive(&mut self.editor, frame, now_ms);
        if let Some(view) = view.as_ref() {
            self.editor.restore_view(view);
        }
        Ok(result?)
    }

    pub fn base_ready(&mut self, now_ms: u64) -> Result<Step, GridLiveError> {
        let view = self.editor.capture_view();
        let result = self.client.base_ready(&mut self.editor, now_ms);
        if let Some(view) = view.as_ref() {
            self.editor.restore_view(view);
        }
        Ok(result?)
    }

    pub fn tick(&mut self, now_ms: u64) -> Vec<String> {
        let mut send = std::mem::take(&mut self.outbox);
        send.extend(self.client.tick(now_ms));
        send
    }
}
