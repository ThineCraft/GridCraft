//! Live co-authoring: the `gridcraft:live-*` messages between a ShareCraft page and GridCraft.
//!
//! The page owns the socket, the back-off and the HTTP calls; GridCraft owns the live client.
//! Every message goes between same-origin windows by `postMessage`: the editor hears only its
//! parent, on its own origin.
//!
//! Page to editor:
//! - `gridcraft:live-start` `{ document, format, canEdit }`: begin a live session for the
//!   document (`format` must be GridCraft's live format; `canEdit`: this tab may edit).
//! - `gridcraft:live-open`, `gridcraft:live-closed`: the socket opened or closed.
//! - `gridcraft:live-frame` `{ frame }`: one server frame as text, at most [`MAX_FRAME_BYTES`].
//! - `gridcraft:live-tick`: sent every minute and on visibility change; answered with a state.
//! - `gridcraft:live-base` `{ revision, at, bytes, sessionData, stamp: { atMs, seed } }`: the base
//!   the editor asked for; `sessionData` and `stamp` come together, for a checkpoint.
//! - `gridcraft:live-people` `{ people: [{ participant, name, initials, colour, connected }] }`,
//!   at most [`MAX_PEOPLE`].
//! - `gridcraft:live-stop`: leave now.
//!
//! Editor to page:
//! - `gridcraft:live-send` `{ frame }`: send this frame on the socket.
//! - `gridcraft:live-reset` `{ kind }`: close and reconnect after back-off; only the error's kind.
//! - `gridcraft:live-load` `{ revision, at }`: fetch this base and send `live-base`.
//! - `gridcraft:live-checkpoint` `{ ticket, at, base, bytes, sessionData, sha256 }`: upload it.
//! - `gridcraft:live-hash` `{ ticket, at, sha256 }`: report the verifying hash.
//! - `gridcraft:live-notice` `{ notice }`: one notice to show, as the engine's `LiveNotice` JSON.
//! - `gridcraft:live-state` `{ phase, waiting, canEdit }`: answers every tick, and is sent when it
//!   changes.
//!
//! Frames, ticks and tickets are handled in the message handler ([`LiveSeat::handle`]), never
//! waiting for a frame to be drawn, so a hidden tab keeps its place and stays up to date. The
//! parsing here works on `serde_json::Value`, so it is tested without a browser; the web-sys layer
//! ([`web`]) only converts and posts.
//!
//! Nothing here logs a name, a frame, a payload or typed text: log lines name kinds and positions.

use std::fmt;
use std::sync::Arc;

use gridcraft_engine::coauthor::{
    ClientError, Event, GRIDCRAFT_LIVE_FORMAT, GridCoauthor, GridLiveEditor, GridLiveError, LiveBase, LiveConfig, MAX_SESSION_DATA_BYTES, Phase, Seq,
    Stamp, Step, TicketAnswer,
};
use serde_json::{Value as Json, json};

/// The largest server frame taken from the page.
pub const MAX_FRAME_BYTES: usize = 1 << 20;
/// The largest base file taken from the page, as for an ordinary open.
pub const MAX_BASE_BYTES: usize = 512 << 20;
/// The most people a people list may name.
pub const MAX_PEOPLE: usize = 256;
/// The longest document id.
pub const MAX_DOCUMENT_ID_CHARS: usize = 128;
/// The longest stored revision name.
pub const MAX_REVISION_CHARS: usize = 128;
/// The longest person's name.
pub const MAX_NAME_CHARS: usize = 200;
/// The longest initials.
pub const MAX_INITIALS_CHARS: usize = 8;
/// The longest colour.
pub const MAX_COLOUR_CHARS: usize = 32;
/// The longest format name.
const MAX_FORMAT_CHARS: usize = 64;

/// The crash note's key in the frame's session storage, before the document id.
pub const CRASH_NOTE_PREFIX: &str = "gridcraft-live-applying:";

/// The prefix every live message type carries.
const LIVE_PREFIX: &str = "gridcraft:live-";

/// A message from the page.
pub enum LivePageMsg {
    Start { document: String, format: String, can_edit: bool },
    Open,
    Closed,
    Frame(String),
    Tick,
    Base(PageBase),
    People(Vec<PagePerson>),
    Stop,
}

/// Debug names the message and sizes, never a frame, a name or file content.
impl fmt::Debug for LivePageMsg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Start { document, format, can_edit } => {
                f.debug_struct("Start").field("document_len", &document.len()).field("format", format).field("can_edit", can_edit).finish()
            }
            Self::Open => f.write_str("Open"),
            Self::Closed => f.write_str("Closed"),
            Self::Frame(frame) => f.debug_struct("Frame").field("len", &frame.len()).finish(),
            Self::Tick => f.write_str("Tick"),
            Self::Base(base) => f
                .debug_struct("Base")
                .field("at", &base.at)
                .field("bytes", &base.bytes.len())
                .field("session_data", &base.session_data.as_ref().map(Vec::len))
                .finish(),
            Self::People(people) => f.debug_list().entries(people).finish(),
            Self::Stop => f.write_str("Stop"),
        }
    }
}

/// The base the editor asked for.
pub struct PageBase {
    pub revision: Option<String>,
    pub at: u64,
    pub bytes: Vec<u8>,
    pub session_data: Option<Vec<u8>>,
    /// The checkpoint's stamp (`at_ms`, `seed`), given with its session data.
    pub stamp: Option<(i64, u32)>,
}

/// One person in the room, as the page names them for the bar and the grid.
#[derive(Clone, PartialEq, Eq)]
pub struct PagePerson {
    pub participant: u32,
    pub name: String,
    pub initials: String,
    pub colour: String,
    pub connected: bool,
}

/// Debug shows lengths in place of the name and initials.
impl fmt::Debug for PagePerson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PagePerson")
            .field("participant", &self.participant)
            .field("name_len", &self.name.chars().count())
            .field("initials_len", &self.initials.chars().count())
            .field("connected", &self.connected)
            .finish()
    }
}

/// The binary fields of a page message, taken from the JavaScript object apart from the rest
/// (they are never turned into JSON).
#[derive(Default)]
pub struct PageBinaries {
    pub bytes: Option<Vec<u8>>,
    pub session_data: Option<Vec<u8>>,
}

/// Why a live message was refused. Names the field only, never its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveMsgError {
    Invalid(&'static str),
    TooLarge(&'static str),
}

impl fmt::Display for LiveMsgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(field) => write!(f, "invalid `{field}`"),
            Self::TooLarge(field) => write!(f, "`{field}` is too large"),
        }
    }
}

/// Whether a message's `type` is a live message this module reads.
pub fn is_live_type(kind: &str) -> bool {
    kind.starts_with(LIVE_PREFIX)
}

/// Refuses a base over the limits before its bytes are copied (sizes as the browser reports them).
pub fn check_base_sizes(bytes: f64, session_data: Option<f64>) -> Result<(), LiveMsgError> {
    if !(0.0..=MAX_BASE_BYTES as f64).contains(&bytes) {
        return Err(LiveMsgError::TooLarge("bytes"));
    }
    if session_data.is_some_and(|len| !(0.0..=MAX_SESSION_DATA_BYTES as f64).contains(&len)) {
        return Err(LiveMsgError::TooLarge("sessionData"));
    }
    Ok(())
}

/// Reads one page message. `Ok(None)` for anything that is not a known live message, which is
/// ignored; `Err` for a live message whose fields are missing, malformed or over a limit.
pub fn parse_page_msg(data: &Json, binaries: PageBinaries) -> Result<Option<LivePageMsg>, LiveMsgError> {
    let Some(kind) = data.get("type").and_then(Json::as_str).and_then(|kind| kind.strip_prefix(LIVE_PREFIX)) else {
        return Ok(None);
    };
    let message = match kind {
        "start" => LivePageMsg::Start {
            document: document_id(data)?,
            format: text(data, "format", MAX_FORMAT_CHARS)?,
            can_edit: data.get("canEdit").and_then(Json::as_bool).ok_or(LiveMsgError::Invalid("canEdit"))?,
        },
        "open" => LivePageMsg::Open,
        "closed" => LivePageMsg::Closed,
        "tick" => LivePageMsg::Tick,
        "stop" => LivePageMsg::Stop,
        "frame" => {
            let frame = data.get("frame").and_then(Json::as_str).ok_or(LiveMsgError::Invalid("frame"))?;
            if frame.len() > MAX_FRAME_BYTES {
                return Err(LiveMsgError::TooLarge("frame"));
            }
            LivePageMsg::Frame(frame.to_string())
        }
        "base" => LivePageMsg::Base(base(data, binaries)?),
        "people" => LivePageMsg::People(people(data)?),
        _ => return Ok(None),
    };
    Ok(Some(message))
}

fn text(data: &Json, field: &'static str, max_chars: usize) -> Result<String, LiveMsgError> {
    let value = data.get(field).and_then(Json::as_str).ok_or(LiveMsgError::Invalid(field))?;
    if value.chars().count() > max_chars {
        return Err(LiveMsgError::TooLarge(field));
    }
    Ok(value.to_string())
}

/// A document id names a storage key: letters, digits, `-` and `_` only.
fn document_id(data: &Json) -> Result<String, LiveMsgError> {
    let id = text(data, "document", MAX_DOCUMENT_ID_CHARS)?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(LiveMsgError::Invalid("document"));
    }
    Ok(id)
}

fn base(data: &Json, binaries: PageBinaries) -> Result<PageBase, LiveMsgError> {
    let revision = match data.get("revision") {
        None | Some(Json::Null) => None,
        Some(_) => Some(text(data, "revision", MAX_REVISION_CHARS)?),
    };
    let at = data.get("at").and_then(Json::as_u64).ok_or(LiveMsgError::Invalid("at"))?;
    let bytes = binaries.bytes.ok_or(LiveMsgError::Invalid("bytes"))?;
    check_base_sizes(bytes.len() as f64, binaries.session_data.as_ref().map(|data| data.len() as f64))?;
    let stamp = match data.get("stamp") {
        None | Some(Json::Null) => None,
        Some(stamp) => {
            let at_ms = stamp.get("atMs").and_then(Json::as_i64).ok_or(LiveMsgError::Invalid("stamp"))?;
            let seed = stamp.get("seed").and_then(Json::as_u64).and_then(|seed| u32::try_from(seed).ok()).ok_or(LiveMsgError::Invalid("stamp"))?;
            Some((at_ms, seed))
        }
    };
    if binaries.session_data.is_some() != stamp.is_some() {
        return Err(LiveMsgError::Invalid("stamp"));
    }
    Ok(PageBase { revision, at, bytes, session_data: binaries.session_data, stamp })
}

fn people(data: &Json) -> Result<Vec<PagePerson>, LiveMsgError> {
    let list = data.get("people").and_then(Json::as_array).ok_or(LiveMsgError::Invalid("people"))?;
    if list.len() > MAX_PEOPLE {
        return Err(LiveMsgError::TooLarge("people"));
    }
    list.iter()
        .map(|person| {
            Ok(PagePerson {
                participant: person
                    .get("participant")
                    .and_then(Json::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .ok_or(LiveMsgError::Invalid("participant"))?,
                name: text(person, "name", MAX_NAME_CHARS)?,
                initials: text(person, "initials", MAX_INITIALS_CHARS)?,
                colour: text(person, "colour", MAX_COLOUR_CHARS)?,
                connected: person.get("connected").and_then(Json::as_bool).ok_or(LiveMsgError::Invalid("connected"))?,
            })
        })
        .collect()
}

/// A message to the page.
#[derive(Clone, PartialEq, Eq)]
pub enum LiveEditorMsg {
    Send(String),
    Reset { kind: &'static str },
    Load { revision: Option<String>, at: u64 },
    Checkpoint { ticket: String, at: u64, base: Option<String>, bytes: Vec<u8>, session_data: Vec<u8>, sha256: String },
    Hash { ticket: String, at: u64, sha256: String },
    Notice(Json),
    State { phase: &'static str, waiting: usize, can_edit: bool },
}

/// Debug names the message, positions and sizes; frames, notices and file content are left out.
impl fmt::Debug for LiveEditorMsg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Send(frame) => f.debug_struct("Send").field("len", &frame.len()).finish(),
            Self::Reset { kind } => f.debug_struct("Reset").field("kind", kind).finish(),
            Self::Load { at, .. } => f.debug_struct("Load").field("at", at).finish(),
            Self::Checkpoint { at, bytes, session_data, .. } => {
                f.debug_struct("Checkpoint").field("at", at).field("bytes", &bytes.len()).field("session_data", &session_data.len()).finish()
            }
            Self::Hash { at, .. } => f.debug_struct("Hash").field("at", at).finish(),
            Self::Notice(notice) => f.debug_struct("Notice").field("kind", &notice.get("kind").and_then(Json::as_str)).finish(),
            Self::State { phase, waiting, can_edit } => {
                f.debug_struct("State").field("phase", phase).field("waiting", waiting).field("can_edit", can_edit).finish()
            }
        }
    }
}

impl LiveEditorMsg {
    /// The message's `type`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Send(_) => "gridcraft:live-send",
            Self::Reset { .. } => "gridcraft:live-reset",
            Self::Load { .. } => "gridcraft:live-load",
            Self::Checkpoint { .. } => "gridcraft:live-checkpoint",
            Self::Hash { .. } => "gridcraft:live-hash",
            Self::Notice(_) => "gridcraft:live-notice",
            Self::State { .. } => "gridcraft:live-state",
        }
    }

    /// The message's fields other than `type` and its binary ones.
    pub fn fields(&self) -> Json {
        match self {
            Self::Send(frame) => json!({ "frame": frame }),
            Self::Reset { kind } => json!({ "kind": kind }),
            Self::Load { revision, at } => json!({ "revision": revision, "at": at }),
            Self::Checkpoint { ticket, at, base, sha256, .. } => json!({ "ticket": ticket, "at": at, "base": base, "sha256": sha256 }),
            Self::Hash { ticket, at, sha256 } => json!({ "ticket": ticket, "at": at, "sha256": sha256 }),
            Self::Notice(notice) => json!({ "notice": notice }),
            Self::State { phase, waiting, can_edit } => json!({ "phase": phase, "waiting": waiting, "canEdit": can_edit }),
        }
    }

    /// The binary fields, posted as `Uint8Array`s.
    pub fn binaries(&self) -> Vec<(&'static str, &[u8])> {
        match self {
            Self::Checkpoint { bytes, session_data, .. } => vec![("bytes", bytes.as_slice()), ("sessionData", session_data.as_slice())],
            _ => Vec::new(),
        }
    }
}

/// Where the crash note is kept: the frame's session storage on the web. A failure to store is
/// logged without content and does not stop the session.
pub trait CrashNotes: Send + Sync {
    fn read(&self, key: &str) -> Option<String>;
    fn write(&self, key: &str, value: &str);
    fn remove(&self, key: &str);
}

/// The live seat: the live client and editor while a session runs, beside the egui app.
pub struct LiveSeat {
    notes: Arc<dyn CrashNotes>,
    coauthor: Option<GridCoauthor>,
    /// Whether the page lets this tab edit; the room decides as well.
    page_may_edit: bool,
    /// The base the editor asked for and waits for.
    expected_base: Option<(Option<String>, u64)>,
    people: Vec<PagePerson>,
    crashed_at: Option<u64>,
    last_state: Option<LiveEditorMsg>,
}

impl LiveSeat {
    pub fn new(notes: Arc<dyn CrashNotes>) -> Self {
        let idle = LiveEditorMsg::State { phase: "idle", waiting: 0, can_edit: false };
        Self { notes, coauthor: None, page_may_edit: false, expected_base: None, people: Vec::new(), crashed_at: None, last_state: Some(idle) }
    }

    /// Whether a live session runs.
    pub fn is_active(&self) -> bool {
        self.coauthor.is_some()
    }

    #[allow(dead_code, reason = "the screen reads and drives the seat (G5 task 11)")]
    pub fn coauthor(&self) -> Option<&GridCoauthor> {
        self.coauthor.as_ref()
    }

    #[allow(dead_code, reason = "the screen reads and drives the seat (G5 task 11)")]
    pub fn coauthor_mut(&mut self) -> Option<&mut GridCoauthor> {
        self.coauthor.as_mut()
    }

    /// The position the editor stopped in before this load, from the crash note.
    #[allow(dead_code, reason = "the screen reads and drives the seat (G5 task 11)")]
    pub fn crashed_at(&self) -> Option<u64> {
        self.crashed_at
    }

    /// The room's people as the page named them.
    #[allow(dead_code, reason = "the screen reads and drives the seat (G5 task 11)")]
    pub fn people(&self) -> &[PagePerson] {
        &self.people
    }

    /// Handles one page message at `now_ms` (a monotonic clock) and returns what to post.
    pub fn handle(&mut self, message: LivePageMsg, now_ms: u64) -> Vec<LiveEditorMsg> {
        let mut out = Vec::new();
        let tick = matches!(message, LivePageMsg::Tick);
        match message {
            LivePageMsg::Start { document, format, can_edit } => self.start(&document, &format, can_edit, now_ms, &mut out),
            LivePageMsg::People(people) => self.people = people,
            LivePageMsg::Stop => self.stop(&mut out),
            other => self.handle_live(other, now_ms, &mut out),
        }
        self.flush(now_ms, &mut out);
        let state = self.state();
        if tick || self.last_state.as_ref() != Some(&state) {
            self.last_state = Some(state.clone());
            out.push(state);
        }
        out
    }

    fn handle_live(&mut self, message: LivePageMsg, now_ms: u64, out: &mut Vec<LiveEditorMsg>) {
        let Some(coauthor) = self.coauthor.as_mut() else {
            if !matches!(message, LivePageMsg::Tick) {
                log::info!("live: {} before the session started; ignored", message_name(&message));
            }
            return;
        };
        match message {
            LivePageMsg::Open => {
                let frames = coauthor.connected(now_ms);
                out.extend(frames.into_iter().map(LiveEditorMsg::Send));
            }
            LivePageMsg::Closed => coauthor.disconnected(now_ms),
            LivePageMsg::Frame(frame) => {
                let result = coauthor.receive(&frame, now_ms);
                self.after(result, out);
            }
            LivePageMsg::Base(base) => self.load_base(base, now_ms, out),
            LivePageMsg::Tick | LivePageMsg::Start { .. } | LivePageMsg::People(_) | LivePageMsg::Stop => {}
        }
    }

    fn start(&mut self, document: &str, format: &str, can_edit: bool, now_ms: u64, out: &mut Vec<LiveEditorMsg>) {
        if self.coauthor.is_some() {
            log::warn!("live: a second start while a session runs; ignored");
            return;
        }
        if format != GRIDCRAFT_LIVE_FORMAT {
            log::warn!("live: the page expects another format; not started");
            out.push(LiveEditorMsg::Notice(json!({ "kind": "incompatible" })));
            return;
        }
        let key = format!("{CRASH_NOTE_PREFIX}{document}");
        self.crashed_at = self.notes.read(&key).and_then(|note| note.trim().parse::<u64>().ok());
        let config = LiveConfig { format: GRIDCRAFT_LIVE_FORMAT.to_string(), crashed_at: self.crashed_at.map(Seq) };
        let editor = match GridLiveEditor::blank() {
            Ok(editor) => editor,
            Err(_) => {
                log::error!("live: the editor could not be made");
                out.push(LiveEditorMsg::Reset { kind: "editor" });
                return;
            }
        };
        let mut coauthor = GridCoauthor::new(config, editor);
        // The socket is not open yet: the join goes when the page says it is.
        coauthor.disconnected(now_ms);
        let _ = coauthor.tick(now_ms);
        let notes = Arc::clone(&self.notes);
        coauthor.set_crash_note(Box::new(move |seq: Option<Seq>| match seq {
            Some(seq) => notes.write(&key, &seq.0.to_string()),
            None => notes.remove(&key),
        }));
        self.page_may_edit = can_edit;
        self.expected_base = None;
        self.coauthor = Some(coauthor);
    }

    fn stop(&mut self, out: &mut Vec<LiveEditorMsg>) {
        if let Some(mut coauthor) = self.coauthor.take() {
            if let Some(frame) = coauthor.leave() {
                out.push(LiveEditorMsg::Send(frame));
            }
            // The session comes back unshared; the screen takes it over (task 11).
            drop(coauthor.finish());
        }
        self.expected_base = None;
    }

    fn load_base(&mut self, base: PageBase, now_ms: u64, out: &mut Vec<LiveEditorMsg>) {
        let Some(coauthor) = self.coauthor.as_mut() else { return };
        if self.expected_base.as_ref() != Some(&(base.revision.clone(), base.at)) {
            log::warn!("live: a base for position {} that was not asked for; ignored", base.at);
            return;
        }
        let loaded = match (&base.session_data, base.stamp) {
            (Some(session_data), Some((at_ms, seed))) => {
                coauthor.load_base(LiveBase::Checkpoint { bytes: &base.bytes, session_data, stamp: Stamp { at_ms, seed } })
            }
            _ if base.revision.is_none() && base.bytes.is_empty() => coauthor.load_base(LiveBase::Empty),
            _ => coauthor.load_base(LiveBase::File { bytes: &base.bytes }),
        };
        if loaded.is_err() {
            log::warn!("live: the base for position {} could not be opened", base.at);
            out.push(LiveEditorMsg::Reset { kind: "base" });
            return;
        }
        self.expected_base = None;
        let result = coauthor.base_ready(now_ms);
        self.after(result, out);
    }

    /// Turns one step into messages: frames to send, the base to fetch, tickets to answer.
    fn after(&mut self, result: Result<Step, GridLiveError>, out: &mut Vec<LiveEditorMsg>) {
        let step = match result {
            Ok(step) => step,
            Err(error) => {
                let kind = reset_kind(&error);
                log::warn!("live: the editor cannot follow the room ({kind}); reconnecting");
                out.push(LiveEditorMsg::Reset { kind });
                return;
            }
        };
        out.extend(step.send.iter().cloned().map(LiveEditorMsg::Send));
        let Some(coauthor) = self.coauthor.as_ref() else { return };
        for event in &step.events {
            match event {
                Event::LoadBase { revision, at } => {
                    self.expected_base = Some((revision.clone(), at.0));
                    out.push(LiveEditorMsg::Load { revision: revision.clone(), at: at.0 });
                }
                Event::CheckpointRequested { .. } | Event::VerifyRequested { .. } => {
                    if let Some(answer) = coauthor.answer_ticket(event) {
                        out.extend(ticket_messages(answer));
                    }
                }
                Event::Expired => out.push(LiveEditorMsg::Reset { kind: "expired" }),
                _ => {}
            }
        }
    }

    /// Frames waiting in the client (heartbeats, unsent changes) and notices for the page.
    fn flush(&mut self, now_ms: u64, out: &mut Vec<LiveEditorMsg>) {
        let Some(coauthor) = self.coauthor.as_mut() else { return };
        out.extend(coauthor.tick(now_ms).into_iter().map(LiveEditorMsg::Send));
        if let Some(frame) = coauthor.presence(now_ms).and_then(presence_frame_stand_in) {
            out.push(LiveEditorMsg::Send(frame));
        }
        for notice in coauthor.take_notices() {
            match serde_json::to_value(&notice) {
                Ok(notice) => out.push(LiveEditorMsg::Notice(notice)),
                Err(_) => log::warn!("live: a notice could not be written"),
            }
        }
    }

    fn state(&self) -> LiveEditorMsg {
        match &self.coauthor {
            None => LiveEditorMsg::State { phase: "idle", waiting: 0, can_edit: false },
            Some(coauthor) => LiveEditorMsg::State {
                phase: phase_name(coauthor.phase()),
                waiting: coauthor.waiting(),
                can_edit: self.page_may_edit && coauthor.can_edit(),
            },
        }
    }
}

/// STAND-IN for ShareCraft G5 task 1. The live wire has no presence message yet
/// (`LiveClientMsg::Presence`, sent through `LiveClient::presence`), so a note cannot be framed
/// and nothing is sent. Replace this with the client's presence frame once task 1 lands.
fn presence_frame_stand_in(note: String) -> Option<String> {
    drop(note);
    None
}

fn ticket_messages(answer: TicketAnswer) -> Vec<LiveEditorMsg> {
    match answer {
        TicketAnswer::Write { ticket, at, base, checkpoint, send } => {
            let mut out = vec![LiveEditorMsg::Checkpoint {
                ticket,
                at: at.0,
                base,
                sha256: hex(&checkpoint.sha256),
                bytes: checkpoint.bytes,
                session_data: checkpoint.session_data,
            }];
            out.extend(send.into_iter().map(LiveEditorMsg::Send));
            out
        }
        TicketAnswer::Verify { ticket, at, sha256, send } => {
            let mut out = vec![LiveEditorMsg::Hash { ticket, at: at.0, sha256: hex(&sha256) }];
            out.extend(send.into_iter().map(LiveEditorMsg::Send));
            out
        }
        TicketAnswer::Failed { send } => {
            log::warn!("live: a checkpoint ticket could not be honoured");
            send.into_iter().map(LiveEditorMsg::Send).collect()
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The kind of an error that makes the page reconnect; never its text.
fn reset_kind(error: &GridLiveError) -> &'static str {
    match error {
        GridLiveError::Client(ClientError::Malformed(_)) => "malformed",
        GridLiveError::Client(ClientError::OutOfOrder { .. }) => "out_of_order",
        GridLiveError::Client(ClientError::Desync(_)) => "desync",
        GridLiveError::Client(ClientError::AgreedUnavailable { .. }) => "agreed_unavailable",
        _ => "editor",
    }
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Offline => "offline",
        Phase::Joining => "joining",
        Phase::LoadingBase => "loading_base",
        Phase::CatchingUp => "catching_up",
        Phase::Live => "live",
        Phase::Resyncing => "resyncing",
        Phase::Stalled { .. } => "stalled",
        Phase::Closed => "closed",
    }
}

fn message_name(message: &LivePageMsg) -> &'static str {
    match message {
        LivePageMsg::Start { .. } => "start",
        LivePageMsg::Open => "open",
        LivePageMsg::Closed => "closed",
        LivePageMsg::Frame(_) => "frame",
        LivePageMsg::Tick => "tick",
        LivePageMsg::Base(_) => "base",
        LivePageMsg::People(_) => "people",
        LivePageMsg::Stop => "stop",
    }
}

/// The browser side: hears the parent's live messages, converts them, and posts the answers.
/// Nothing here decides anything; [`LiveSeat`] does.
#[cfg(target_arch = "wasm32")]
pub mod web {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::sync::Arc;

    use serde_json::{Map, Value as Json};
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::{JsCast as _, JsValue};

    use super::{
        CrashNotes, LiveEditorMsg, LiveMsgError, LivePageMsg, LiveSeat, MAX_FRAME_BYTES, MAX_PEOPLE, PageBinaries, check_base_sizes, is_live_type,
        parse_page_msg,
    };

    /// The most messages held while the app has the seat; a page that floods is cut off.
    const MAX_QUEUED: usize = 4096;

    /// The scalar fields a live message may carry, besides `type`.
    const FIELDS: &[&str] = &["document", "format", "canEdit", "frame", "revision", "at"];
    const PERSON_FIELDS: &[&str] = &["participant", "name", "initials", "colour", "connected"];

    /// The crash note in the frame's session storage. A storage failure is logged without content.
    pub struct SessionNotes;

    fn session_storage() -> Option<web_sys::Storage> {
        web_sys::window()?.session_storage().ok()?
    }

    impl CrashNotes for SessionNotes {
        fn read(&self, key: &str) -> Option<String> {
            session_storage()?.get_item(key).ok()?
        }

        fn write(&self, key: &str, value: &str) {
            if session_storage().is_none_or(|storage| storage.set_item(key, value).is_err()) {
                log::warn!("live: the crash note could not be stored");
            }
        }

        fn remove(&self, key: &str) {
            if session_storage().is_none_or(|storage| storage.remove_item(key).is_err()) {
                log::warn!("live: the crash note could not be cleared");
            }
        }
    }

    struct Shared {
        seat: RefCell<LiveSeat>,
        queue: RefCell<VecDeque<LivePageMsg>>,
        parent: web_sys::Window,
        origin: String,
        ctx: egui::Context,
    }

    /// The live seat beside the egui app, shared with the message handler.
    #[derive(Clone)]
    pub struct LiveHost {
        shared: Rc<Shared>,
    }

    impl LiveHost {
        /// Listens to a same-origin parent window; `None` without one.
        pub fn connect(ctx: egui::Context) -> Option<Self> {
            let window = web_sys::window()?;
            let parent = window.parent().ok().flatten()?;
            if js_sys::Object::is(&parent, &window) {
                return None;
            }
            let origin = window.location().origin().ok()?;
            if parent.location().origin().ok()? != origin {
                return None;
            }
            let shared = Rc::new(Shared {
                seat: RefCell::new(LiveSeat::new(Arc::new(SessionNotes))),
                queue: RefCell::default(),
                parent: parent.clone(),
                origin: origin.clone(),
                ctx,
            });
            let host = Self { shared };
            let listener = host.clone();
            let on_message = Closure::<dyn Fn(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
                if event.origin() != origin || !event.source().is_some_and(|source| js_sys::Object::is(&source, &parent)) {
                    return;
                }
                listener.hear(&event.data());
            });
            if window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_err() {
                return None;
            }
            on_message.forget();
            Some(host)
        }

        /// Whether a live session runs.
        pub fn is_active(&self) -> bool {
            self.shared.seat.try_borrow().is_ok_and(|seat| seat.is_active())
        }

        /// The seat, for the screen (task 11). Borrow it only within a frame.
        #[allow(dead_code, reason = "the screen reads and drives the seat (G5 task 11)")]
        pub fn seat(&self) -> &RefCell<LiveSeat> {
            &self.shared.seat
        }

        /// Handles messages queued while the app held the seat; called at the start of a frame.
        pub fn drain(&self) {
            let Ok(mut seat) = self.shared.seat.try_borrow_mut() else { return };
            loop {
                let next = self.shared.queue.borrow_mut().pop_front();
                let Some(message) = next else { break };
                for out in seat.handle(message, now_ms()) {
                    self.post(&out);
                }
            }
        }

        fn hear(&self, data: &JsValue) {
            let message = match read(data) {
                Ok(Some(message)) => message,
                Ok(None) => return,
                Err(error) => {
                    log::warn!("live: a page message was refused: {error}");
                    return;
                }
            };
            {
                let mut queue = self.shared.queue.borrow_mut();
                if queue.len() >= MAX_QUEUED {
                    log::warn!("live: too many page messages waiting; one was dropped");
                    return;
                }
                queue.push_back(message);
            }
            self.drain();
            self.shared.ctx.request_repaint();
        }

        fn post(&self, message: &LiveEditorMsg) {
            if let Err(error) = post_to(&self.shared.parent, &self.shared.origin, message) {
                log::warn!("live: {} could not be posted ({error})", message.kind());
            }
        }
    }

    /// The page's clock: monotonic, in milliseconds.
    fn now_ms() -> u64 {
        let now = web_sys::window().and_then(|window| window.performance()).map_or(0.0, |performance| performance.now());
        if now.is_finite() && now > 0.0 { now as u64 } else { 0 }
    }

    fn post_to(parent: &web_sys::Window, origin: &str, message: &LiveEditorMsg) -> Result<(), String> {
        let error = |e: JsValue| e.as_string().unwrap_or_else(|| "a browser error".to_string());
        let object = js_sys::JSON::parse(&message.fields().to_string()).map_err(error)?;
        js_sys::Reflect::set(&object, &"type".into(), &message.kind().into()).map_err(error)?;
        let transfer = js_sys::Array::new();
        for (name, bytes) in message.binaries() {
            let array = js_sys::Uint8Array::from(bytes);
            transfer.push(&array.buffer());
            js_sys::Reflect::set(&object, &name.into(), &array).map_err(error)?;
        }
        if transfer.length() == 0 {
            parent.post_message(&object, origin).map_err(error)
        } else {
            parent.post_message_with_transfer(&object, origin, &transfer).map_err(error)
        }
    }

    /// Reads a page message: its scalar fields into JSON (bounded before anything is copied), its
    /// file bytes apart. `Ok(None)` for anything that is not a live message.
    fn read(data: &JsValue) -> Result<Option<LivePageMsg>, LiveMsgError> {
        let Some(kind) = js_sys::Reflect::get(data, &"type".into()).ok().and_then(|kind| kind.as_string()) else { return Ok(None) };
        if !is_live_type(&kind) {
            return Ok(None);
        }
        let mut map = scalars(data, FIELDS)?;
        map.insert("type".into(), Json::String(kind.clone()));
        if let Some(stamp) = field(data, "stamp").filter(JsValue::is_object) {
            map.insert("stamp".into(), Json::Object(scalars(&stamp, &["atMs", "seed"])?));
        }
        if let Some(people) = field(data, "people") {
            map.insert("people".into(), people_json(&people)?);
        }
        let binaries = if kind == "gridcraft:live-base" { base_binaries(data)? } else { PageBinaries::default() };
        parse_page_msg(&Json::Object(map), binaries)
    }

    fn field(data: &JsValue, name: &str) -> Option<JsValue> {
        js_sys::Reflect::get(data, &name.into()).ok().filter(|value| !value.is_undefined())
    }

    fn scalars(data: &JsValue, names: &[&'static str]) -> Result<Map<String, Json>, LiveMsgError> {
        let mut map = Map::new();
        for name in names {
            if let Some(value) = field(data, name) {
                map.insert((*name).to_string(), scalar(&value, name)?);
            }
        }
        Ok(map)
    }

    /// One scalar: a string is measured before it is copied (UTF-16 units never exceed UTF-8
    /// bytes, so a longer one is over the frame limit); anything else that is not a number,
    /// boolean or null becomes null and fails the field's check.
    fn scalar(value: &JsValue, name: &'static str) -> Result<Json, LiveMsgError> {
        if let Some(text) = value.dyn_ref::<js_sys::JsString>() {
            if text.length() as usize > MAX_FRAME_BYTES {
                return Err(LiveMsgError::TooLarge(name));
            }
            return Ok(value.as_string().map_or(Json::Null, Json::String));
        }
        if let Some(number) = value.as_f64() {
            const SAFE: f64 = 9_007_199_254_740_991.0;
            if number.fract() == 0.0 && number.abs() <= SAFE {
                return Ok(Json::from(number as i64));
            }
            return Ok(serde_json::Number::from_f64(number).map_or(Json::Null, Json::Number));
        }
        Ok(value.as_bool().map_or(Json::Null, Json::Bool))
    }

    fn people_json(people: &JsValue) -> Result<Json, LiveMsgError> {
        let Some(list) = people.dyn_ref::<js_sys::Array>() else { return Ok(Json::Null) };
        if list.length() as usize > MAX_PEOPLE {
            return Err(LiveMsgError::TooLarge("people"));
        }
        list.iter().map(|person| scalars(&person, PERSON_FIELDS).map(Json::Object)).collect::<Result<Vec<_>, _>>().map(Json::Array)
    }

    fn base_binaries(data: &JsValue) -> Result<PageBinaries, LiveMsgError> {
        let bytes = field(data, "bytes").and_then(|value| bytes_view(&value));
        let session_data = field(data, "sessionData").and_then(|value| bytes_view(&value));
        let Some(bytes) = bytes else { return Err(LiveMsgError::Invalid("bytes")) };
        check_base_sizes(f64::from(bytes.length()), session_data.as_ref().map(|data| f64::from(data.length())))?;
        Ok(PageBinaries { bytes: Some(bytes.to_vec()), session_data: session_data.map(|data| data.to_vec()) })
    }

    /// A `Uint8Array`, or a view of an `ArrayBuffer`, without copying.
    fn bytes_view(value: &JsValue) -> Option<js_sys::Uint8Array> {
        if let Some(array) = value.dyn_ref::<js_sys::Uint8Array>() {
            return Some(array.clone());
        }
        value.dyn_ref::<js_sys::ArrayBuffer>().map(|buffer| js_sys::Uint8Array::new(buffer))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use gridcraft_engine::coauthor::{GRIDCRAFT_LIVE_FORMAT, Seq};
    use gridcraft_engine::core::CellRef;
    use gridcraft_engine::core::Value;
    use gridcraft_engine::model::Workbook;
    use serde_json::{Value as Json, json};

    use super::*;

    const DOCUMENT: &str = "doc-1";

    #[derive(Default)]
    struct MemoryNotes {
        notes: Mutex<BTreeMap<String, String>>,
        writes: Mutex<Vec<(String, Option<String>)>>,
    }

    impl CrashNotes for MemoryNotes {
        fn read(&self, key: &str) -> Option<String> {
            self.notes.lock().unwrap().get(key).cloned()
        }

        fn write(&self, key: &str, value: &str) {
            self.notes.lock().unwrap().insert(key.to_string(), value.to_string());
            self.writes.lock().unwrap().push((key.to_string(), Some(value.to_string())));
        }

        fn remove(&self, key: &str) {
            self.notes.lock().unwrap().remove(key);
            self.writes.lock().unwrap().push((key.to_string(), None));
        }
    }

    fn parse(value: Json) -> Result<Option<LivePageMsg>, LiveMsgError> {
        parse_page_msg(&value, PageBinaries::default())
    }

    fn base_bytes() -> Vec<u8> {
        gridcraft_engine::io::save_bytes(&Workbook::new(), "base.xlsx").unwrap()
    }

    fn sends(out: &[LiveEditorMsg]) -> Vec<Json> {
        out.iter()
            .filter_map(|message| match message {
                LiveEditorMsg::Send(frame) => serde_json::from_str(frame).ok(),
                _ => None,
            })
            .collect()
    }

    /// A started seat whose socket opened; returns it with its client id from the join frame.
    fn opened(notes: Arc<MemoryNotes>, now: u64) -> (LiveSeat, String) {
        let mut seat = LiveSeat::new(notes);
        let out = seat.handle(LivePageMsg::Start { document: DOCUMENT.into(), format: GRIDCRAFT_LIVE_FORMAT.into(), can_edit: true }, now);
        assert!(sends(&out).is_empty(), "nothing is sent before the socket opens");
        let out = seat.handle(LivePageMsg::Open, now + 1);
        let join = sends(&out).into_iter().find(|frame| frame["type"] == "live_join").expect("the open socket carries a join");
        (seat, join["client"].as_str().unwrap().to_string())
    }

    /// Joins the seat as `participant` and loads the stored file as its base.
    fn join_live(seat: &mut LiveSeat, participant: u32, now: u64) {
        let joined = json!({
            "type":"live_joined",
            "session":"00000000-0000-0000-0000-00000000005e",
            "participant":participant,
            "place":{"is":"new"},
            "can_edit":true,
            "heartbeat_ms":60_000,
            "people":[]
        });
        seat.handle(LivePageMsg::Frame(joined.to_string()), now);
        let catchup = json!({"type":"catchup","base":0,"revision":"r1","from":0,"head":0});
        let out = seat.handle(LivePageMsg::Frame(catchup.to_string()), now + 1);
        assert!(
            out.iter().any(|message| matches!(message, LiveEditorMsg::Load { revision: Some(r), at: 0 } if r == "r1")),
            "the editor asks the page for the base: {out:?}"
        );
        let base = PageBase { revision: Some("r1".into()), at: 0, bytes: base_bytes(), session_data: None, stamp: None };
        seat.handle(LivePageMsg::Base(base), now + 2);
        let state = seat.handle(LivePageMsg::Tick, now + 3);
        assert!(state.iter().any(|message| matches!(message, LiveEditorMsg::State { phase: "live", .. })), "live after the base: {state:?}");
    }

    fn other_entry(seq: u64, participant: u32, client: &str, sent: &Json) -> String {
        json!({
            "type":"entry",
            "entry":{
                "seq":seq,
                "at_ms":1_795_219_200_000_i64,
                "seed":7,
                "kind":"change",
                "participant":participant,
                "client":client,
                "local":sent["local"],
                "seen":sent["seen"],
                "payload":sent["payload"],
                "cancelled":false
            }
        })
        .to_string()
    }

    /// Alice (participant 2) sets A1 to 41; the server's ordered entry for it.
    fn alices_entry(now: u64) -> String {
        let (mut alice, client) = opened(Arc::default(), now);
        join_live(&mut alice, 2, now);
        let step = alice.coauthor_mut().unwrap().submit("cell.set", json!({"sheet":0,"cell":"A1","input":"41"}), 2026, now + 10).unwrap();
        let sent: Json =
            step.send.iter().filter_map(|frame| serde_json::from_str::<Json>(frame).ok()).find(|frame| frame["type"] == "entry").unwrap();
        other_entry(1, 2, &client, &sent)
    }

    fn a1(seat: &LiveSeat) -> Value {
        seat.coauthor().unwrap().editor().session().doc().unwrap().wb.sheet(0).unwrap().value(CellRef::parse("A1").unwrap())
    }

    fn base_fields_are_bounded() {
        let with_bytes = || PageBinaries { bytes: Some(vec![1, 2]), session_data: None };
        let base = parse_page_msg(&json!({"type":"gridcraft:live-base","revision":"r1","at":3}), with_bytes()).unwrap();
        assert!(
            matches!(base, Some(LivePageMsg::Base(PageBase { at: 3, ref revision, ref bytes, session_data: None, stamp: None })) if revision.as_deref() == Some("r1") && bytes == &[1, 2])
        );
        assert!(parse_page_msg(&json!({"type":"gridcraft:live-base","revision":null,"at":-1}), with_bytes()).is_err());
        assert!(parse_page_msg(&json!({"type":"gridcraft:live-base","revision":null,"at":1.5}), with_bytes()).is_err());
        assert!(parse(json!({"type":"gridcraft:live-base","revision":null,"at":0})).is_err(), "a base carries its file bytes");
        let checkpoint = || PageBinaries { bytes: Some(vec![1]), session_data: Some(vec![2]) };
        assert!(
            parse_page_msg(&json!({"type":"gridcraft:live-base","revision":"r2","at":4}), checkpoint()).is_err(),
            "session data needs the checkpoint's stamp"
        );
        let stamped =
            parse_page_msg(&json!({"type":"gridcraft:live-base","revision":"r2","at":4,"stamp":{"atMs":5,"seed":6}}), checkpoint()).unwrap();
        assert!(matches!(stamped, Some(LivePageMsg::Base(PageBase { stamp: Some((5, 6)), .. }))));
        assert!(check_base_sizes(MAX_BASE_BYTES as f64, Some(MAX_SESSION_DATA_BYTES as f64)).is_ok());
        assert!(check_base_sizes(MAX_BASE_BYTES as f64 + 1.0, None).is_err(), "a base file over 512 MiB is refused before it is copied");
        assert!(check_base_sizes(1.0, Some(MAX_SESSION_DATA_BYTES as f64 + 1.0)).is_err(), "session data over 8 MiB is refused");
    }

    #[test]
    fn live_messages_parse_and_bound_their_fields() {
        let start = parse(json!({"type":"gridcraft:live-start","document":"doc-1","format":"gridcraft/1","canEdit":true})).unwrap();
        assert!(
            matches!(start, Some(LivePageMsg::Start { ref document, ref format, can_edit: true }) if document == "doc-1" && format == "gridcraft/1")
        );
        assert!(
            parse(json!({"type":"gridcraft:live-start","document":"d".repeat(MAX_DOCUMENT_ID_CHARS + 1),"format":"gridcraft/1","canEdit":true}))
                .is_err()
        );
        assert!(parse(json!({"type":"gridcraft:live-start","document":"../x","format":"gridcraft/1","canEdit":true})).is_err());
        assert!(parse(json!({"type":"gridcraft:live-start","document":"doc-1","format":"gridcraft/1","canEdit":"yes"})).is_err());
        assert!(matches!(parse(json!({"type":"gridcraft:live-open"})), Ok(Some(LivePageMsg::Open))));
        assert!(matches!(parse(json!({"type":"gridcraft:live-closed"})), Ok(Some(LivePageMsg::Closed))));
        assert!(matches!(parse(json!({"type":"gridcraft:live-tick"})), Ok(Some(LivePageMsg::Tick))));
        assert!(matches!(parse(json!({"type":"gridcraft:live-stop"})), Ok(Some(LivePageMsg::Stop))));

        base_fields_are_bounded();

        let person = json!({"participant":2,"name":"Ann","initials":"A","colour":"#336699","connected":true});
        let people = parse(json!({"type":"gridcraft:live-people","people":[person]})).unwrap();
        assert!(matches!(people, Some(LivePageMsg::People(ref list)) if list.len() == 1 && list[0].participant == 2 && list[0].connected));
        let long_name = json!({"participant":2,"name":"n".repeat(MAX_NAME_CHARS + 1),"initials":"A","colour":"#336699","connected":true});
        assert!(parse(json!({"type":"gridcraft:live-people","people":[long_name]})).is_err());
        let debug = format!("{people:?}");
        assert!(!debug.contains("Ann"), "a person's name is kept out of Debug: {debug}");
    }

    #[test]
    fn frames_over_a_mebibyte_are_refused() {
        let at_limit = "x".repeat(MAX_FRAME_BYTES);
        assert!(matches!(parse(json!({"type":"gridcraft:live-frame","frame":at_limit})), Ok(Some(LivePageMsg::Frame(_)))));
        let over = "x".repeat(MAX_FRAME_BYTES + 1);
        assert!(matches!(parse(json!({"type":"gridcraft:live-frame","frame":over})), Err(LiveMsgError::TooLarge(_))));
        assert!(parse(json!({"type":"gridcraft:live-frame","frame":7})).is_err());
    }

    #[test]
    fn unknown_live_messages_are_ignored() {
        assert!(matches!(parse(json!({"type":"gridcraft:live-dance"})), Ok(None)));
        assert!(matches!(parse(json!({"type":"gridcraft:open","name":"a.xlsx"})), Ok(None)), "the file contract is not a live message");
        assert!(matches!(parse(json!({"kind":"gridcraft:live-tick"})), Ok(None)));
        assert!(matches!(parse(json!({"type":7})), Ok(None)));
        assert!(matches!(parse(json!("gridcraft:live-tick")), Ok(None)));
        assert!(is_live_type("gridcraft:live-tick") && !is_live_type("gridcraft:open"), "the browser layer converts live messages only");

        let mut seat = LiveSeat::new(Arc::new(MemoryNotes::default()));
        assert!(seat.handle(LivePageMsg::Frame("{}".into()), 1).is_empty(), "a frame before the start is dropped");
        assert!(seat.handle(LivePageMsg::Open, 2).is_empty());
    }

    #[test]
    fn people_lists_over_256_are_refused() {
        let person = |n: usize| json!({"participant":n,"name":"P","initials":"P","colour":"#000000","connected":false});
        let full: Vec<Json> = (0..MAX_PEOPLE).map(person).collect();
        assert!(
            matches!(parse(json!({"type":"gridcraft:live-people","people":full})), Ok(Some(LivePageMsg::People(ref list))) if list.len() == MAX_PEOPLE)
        );
        let over: Vec<Json> = (0..=MAX_PEOPLE).map(person).collect();
        assert!(matches!(parse(json!({"type":"gridcraft:live-people","people":over})), Err(LiveMsgError::TooLarge(_))));
    }

    #[test]
    fn checkpoint_message_names_ticket_position_and_hash() {
        let (mut seat, _) = opened(Arc::default(), 10);
        join_live(&mut seat, 3, 10);
        let expected = seat.coauthor().unwrap().checkpoint(Seq(0)).unwrap();
        let hex: String = expected.sha256.iter().map(|byte| format!("{byte:02x}")).collect();

        let ticket = "00000000-0000-0000-0000-0000000000a1";
        let write = json!({"type":"checkpoint_ticket","ticket":ticket,"purpose":"write","at":0,"base":"r1","major":null});
        let out = seat.handle(LivePageMsg::Frame(write.to_string()), 20);
        let checkpoint = out.iter().find_map(|message| match message {
            LiveEditorMsg::Checkpoint { ticket, at, base, bytes, session_data, sha256 } => Some((ticket, at, base, bytes, session_data, sha256)),
            _ => None,
        });
        let (named, at, base, bytes, session_data, sha256) = checkpoint.expect("a write ticket posts the checkpoint");
        assert_eq!((named.as_str(), *at, base.as_deref()), (ticket, 0, Some("r1")));
        assert_eq!(bytes, &expected.bytes);
        assert_eq!(session_data, &expected.session_data);
        assert_eq!(sha256, &hex);
        let posted = out.iter().find(|message| matches!(message, LiveEditorMsg::Checkpoint { .. })).unwrap();
        assert_eq!(posted.kind(), "gridcraft:live-checkpoint");
        assert_eq!(posted.fields(), json!({"ticket":ticket,"at":0,"base":"r1","sha256":hex}), "file content goes apart from the fields");
        let binaries: Vec<&str> = posted.binaries().into_iter().map(|(name, _)| name).collect();
        assert_eq!(binaries, ["bytes", "sessionData"]);
        assert!(sends(&out).iter().any(|frame| frame["type"] == "fingerprint" && frame["at"] == 0), "the entered fingerprint goes to the room");

        let verify =
            json!({"type":"checkpoint_ticket","ticket":"00000000-0000-0000-0000-0000000000a2","purpose":"verify","at":0,"base":"r1","major":null});
        let out = seat.handle(LivePageMsg::Frame(verify.to_string()), 30);
        assert!(out.iter().any(|message| matches!(message, LiveEditorMsg::Hash { ticket, at: 0, sha256 } if ticket == "00000000-0000-0000-0000-0000000000a2" && *sha256 == hex)));
        assert!(!out.iter().any(|message| matches!(message, LiveEditorMsg::Checkpoint { .. })), "a verify ticket posts only the hash");

        let wrong =
            json!({"type":"checkpoint_ticket","ticket":"00000000-0000-0000-0000-0000000000a3","purpose":"write","at":9,"base":"r1","major":null});
        let out = seat.handle(LivePageMsg::Frame(wrong.to_string()), 40);
        assert!(
            !out.iter().any(|message| matches!(message, LiveEditorMsg::Checkpoint { .. } | LiveEditorMsg::Hash { .. })),
            "a failure posts nothing"
        );
        assert!(sends(&out).iter().any(|frame| frame["type"] == "checkpoint_failed" && frame["ticket"] == "00000000-0000-0000-0000-0000000000a3"));
    }

    #[test]
    fn client_error_asks_the_page_to_reconnect() {
        let (mut seat, _) = opened(Arc::default(), 10);
        let out = seat.handle(LivePageMsg::Frame(r#"{"type":"sentinel-secret-7"}"#.into()), 20);
        assert!(out.iter().any(|message| matches!(message, LiveEditorMsg::Reset { kind: "malformed" })), "{out:?}");
        let shown = format!("{out:?}");
        assert!(!shown.contains("sentinel"), "a reset carries the kind only: {shown}");
        let fields = out.iter().map(LiveEditorMsg::fields).map(|fields| fields.to_string()).collect::<String>();
        assert!(!fields.contains("sentinel"), "nothing from the frame reaches the page: {fields}");
    }

    #[test]
    fn ticks_without_drawing_send_heartbeats() {
        let (mut seat, _) = opened(Arc::default(), 10);
        join_live(&mut seat, 3, 10);
        let out = seat.handle(LivePageMsg::Tick, 10 + 60_000 + 10);
        assert!(sends(&out).iter().any(|frame| frame["type"] == "heartbeat"), "a tick alone sends the heartbeat: {out:?}");
    }

    #[test]
    fn frames_apply_without_drawing() {
        let entry = alices_entry(10);
        let (mut bob, _) = opened(Arc::default(), 10);
        join_live(&mut bob, 3, 10);
        bob.handle(LivePageMsg::Frame(entry), 50);
        assert_eq!(a1(&bob), Value::Number(41.0), "the frame is applied in the handler, with no frame drawn");
    }

    #[test]
    fn crash_note_round_trips() {
        let key = format!("{CRASH_NOTE_PREFIX}{DOCUMENT}");
        let entry = alices_entry(10);
        let notes = Arc::new(MemoryNotes::default());
        let (mut bob, _) = opened(notes.clone(), 10);
        join_live(&mut bob, 3, 10);
        notes.writes.lock().unwrap().clear();
        bob.handle(LivePageMsg::Frame(entry), 50);
        let writes = notes.writes.lock().unwrap().clone();
        assert_eq!(writes, vec![(key.clone(), Some("1".to_string())), (key.clone(), None)], "noted before applying, cleared after");

        let left = Arc::new(MemoryNotes::default());
        left.notes.lock().unwrap().insert(key.clone(), "7".into());
        let (reloaded, _) = opened(left, 60);
        assert_eq!(reloaded.crashed_at(), Some(7), "a note found at start becomes the crash position");

        let garbled = Arc::new(MemoryNotes::default());
        garbled.notes.lock().unwrap().insert(key, "seven".into());
        let (fresh, _) = opened(garbled, 70);
        assert_eq!(fresh.crashed_at(), None, "a note that is not a position is ignored");
    }

    #[test]
    fn every_tick_is_answered_with_state() {
        let mut idle = LiveSeat::new(Arc::new(MemoryNotes::default()));
        let out = idle.handle(LivePageMsg::Tick, 1);
        assert!(matches!(out.as_slice(), [LiveEditorMsg::State { phase: "idle", waiting: 0, can_edit: false }]), "{out:?}");
        assert!(!idle.is_active());

        let (mut seat, _) = opened(Arc::default(), 10);
        join_live(&mut seat, 3, 10);
        assert!(seat.is_active());
        for now in [100, 200, 300] {
            let out = seat.handle(LivePageMsg::Tick, now);
            let states = out.iter().filter(|message| matches!(message, LiveEditorMsg::State { phase: "live", waiting: 0, can_edit: true })).count();
            assert_eq!(states, 1, "each tick gets one state: {out:?}");
        }
    }
}
