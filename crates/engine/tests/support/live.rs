use collab_core::ids::{ClientId, Seq};
use collab_live::client::{EntryView, Event, LiveClient, LiveConfig, Stamp, Step};
use collab_live::ids::LiveId;
use gridcraft_engine::coauthor::{
    GRIDCRAFT_LIVE_FORMAT, GridCheckpoint, GridCoauthor, GridLiveEditor, IdentityDelete, LiveBase, Phase, PreparedChange, SharedCommand, SharedEntry,
    TicketAnswer, WorkbookIds, identity_runs_visited,
};
use gridcraft_engine::{DocState, Session, Trust};
use gridcraft_model::{Sheet, Workbook};
use protocol::live::{LiveClientMsg, LocalNo, ParticipantNo};
use serde_json::{Value as Json, json};
use std::sync::Arc;

pub fn session_with_sheets(names: &[&str]) -> Session {
    let mut workbook = Workbook::new();
    workbook.sheets.clear();
    workbook.sheets.extend(names.iter().map(|name| Arc::new(Sheet::new(*name))));
    let mut session = Session::new();
    session.add_document(DocState::new(workbook, None, "Book1".into(), Trust::Local));
    session
}

pub fn select(session: &mut Session, sheet: &str, cell: &str) {
    session.execute("sheet.activate", json!({"sheet":sheet})).expect("sheet activates");
    session.execute("selection.set", json!({"range":cell})).expect("selection moves");
}

pub fn prepare(editor: &mut GridLiveEditor, action: u64, id: &str, params: Json, next: &mut u32) -> PreparedChange {
    editor
        .prepare(action, id, &params, 2026, &mut |count| {
            let first = LiveId(2, *next);
            *next = next.checked_add(count).ok_or(collab_live::ids::IdError::NoPlace)?;
            Ok(first)
        })
        .expect("shared change prepares")
}

pub fn remote_entry(payload: &str, seq: u64) -> EntryView<'_> {
    EntryView { seq: Some(Seq(seq)), participant: ParticipantNo(2), own: false, stamp: Some(Stamp { at_ms: 1_795_219_200_000, seed: 41 }), payload }
}

pub struct SimulatedServer {
    seq: u64,
}

impl SimulatedServer {
    pub fn start(editor: &mut GridLiveEditor, crashed_at: Option<Seq>, head: u64) -> (Self, LiveClient<GridLiveEditor>) {
        let mut client = LiveClient::new(LiveConfig { format: GRIDCRAFT_LIVE_FORMAT.to_string(), crashed_at });
        client.connected(0);
        client
            .receive(
                editor,
                &json!({
                    "type":"live_joined",
                    "session":"00000000-0000-0000-0000-00000000005e",
                    "participant":1,
                    "place":{"is":"new"},
                    "can_edit":true,
                    "heartbeat_ms":60_000,
                    "people":[]
                })
                .to_string(),
                1,
            )
            .expect("join is accepted");
        let step = client
            .receive(editor, &json!({"type":"catchup","base":0,"revision":null,"from":0,"head":head}).to_string(), 2)
            .expect("catch-up is accepted");
        assert!(step.events.iter().any(|event| matches!(event, Event::LoadBase { .. })));
        (Self { seq: 0 }, client)
    }

    pub fn queue_remote(&mut self, client: &mut LiveClient<GridLiveEditor>, editor: &mut GridLiveEditor, payload: &str) {
        self.seq += 1;
        client
            .receive(
                editor,
                &json!({
                    "type":"entry",
                    "entry":{
                        "seq":self.seq,
                        "at_ms":1_795_219_200_000i64,
                        "seed":41,
                        "kind":"change",
                        "participant":2,
                        "client":"00000000-0000-0000-0000-000000000099",
                        "local":self.seq,
                        "seen":self.seq - 1,
                        "payload":payload,
                        "cancelled":false
                    }
                })
                .to_string(),
                3 + self.seq,
            )
            .expect("remote entry is accepted");
    }
}

#[derive(Clone, Debug)]
pub struct ClientPlace {
    pub client: ClientId,
    pub participant: ParticipantNo,
}

#[derive(Clone, Debug)]
pub struct ProposedEntry {
    pub client: ClientId,
    pub participant: ParticipantNo,
    pub local: LocalNo,
    pub seen: Seq,
    pub payload: String,
}

#[derive(Clone, Debug)]
pub struct OrderedEntry {
    pub seq: Seq,
    pub participant: ParticipantNo,
    pub payload: String,
    pub at_ms: i64,
    pub seed: u32,
    frame: String,
}

/// A deterministic, in-process server for the G2 wire protocol. Tests decide when a proposed
/// entry is ordered and which client receives each resulting frame, so an own acknowledgement
/// can be delayed behind another person's entry without bypassing `LiveClient`.
pub struct DeterministicServer {
    seq: u64,
    next_participant: u32,
}

impl DeterministicServer {
    pub fn new() -> Self {
        Self { seq: 0, next_participant: 1 }
    }

    pub fn join(&mut self, client: &mut GridCoauthor, now_ms: u64) -> ClientPlace {
        let join = client
            .tick(now_ms)
            .into_iter()
            .find_map(|frame| match serde_json::from_str::<LiveClientMsg>(&frame).expect("client frame is valid") {
                LiveClientMsg::LiveJoin { client, .. } => Some(client),
                _ => None,
            })
            .expect("new co-author sends a live join");
        let participant = ParticipantNo(self.next_participant);
        self.next_participant = self.next_participant.checked_add(1).expect("test participant numbers fit");
        client
            .receive(
                &json!({
                    "type":"live_joined",
                    "session":"00000000-0000-0000-0000-00000000005e",
                    "participant":participant.0,
                    "place":{"is":"new"},
                    "can_edit":true,
                    "heartbeat_ms":60_000,
                    "people":[]
                })
                .to_string(),
                now_ms.saturating_add(1),
            )
            .expect("join is accepted");
        let catchup = client
            .receive(&json!({"type":"catchup","base":0,"revision":null,"from":0,"head":self.seq}).to_string(), now_ms.saturating_add(2))
            .expect("catch-up is accepted");
        assert!(catchup.events.iter().any(|event| matches!(event, Event::LoadBase { .. })));
        client.base_ready(now_ms.saturating_add(3)).expect("loaded base is accepted");
        ClientPlace { client: join, participant }
    }

    pub fn proposals(&self, place: &ClientPlace, step: &Step) -> Vec<ProposedEntry> {
        step.send
            .iter()
            .filter_map(|frame| match serde_json::from_str::<LiveClientMsg>(frame).expect("client frame is valid") {
                LiveClientMsg::Entry { local, seen, payload, .. } => {
                    Some(ProposedEntry { client: place.client, participant: place.participant, local, seen, payload })
                }
                _ => None,
            })
            .collect()
    }

    pub fn one_proposal(&self, place: &ClientPlace, step: &Step) -> ProposedEntry {
        let mut proposals = self.proposals(place, step);
        assert_eq!(proposals.len(), 1, "one submit sends one proposal in these tests");
        proposals.remove(0)
    }

    pub fn order(&mut self, proposed: &ProposedEntry, at_ms: i64, seed: u32) -> OrderedEntry {
        self.seq = self.seq.checked_add(1).expect("test sequence numbers fit");
        let seq = Seq(self.seq);
        let frame = json!({
            "type":"entry",
            "entry":{
                "seq":seq.0,
                "at_ms":at_ms,
                "seed":seed,
                "kind":"change",
                "participant":proposed.participant.0,
                "client":proposed.client,
                "local":proposed.local.0,
                "seen":proposed.seen.0,
                "payload":proposed.payload,
                "cancelled":false
            }
        })
        .to_string();
        OrderedEntry { seq, participant: proposed.participant, payload: proposed.payload.clone(), at_ms, seed, frame }
    }

    pub fn deliver(&self, client: &mut GridCoauthor, entry: &OrderedEntry, now_ms: u64) -> Step {
        client.receive(&entry.frame, now_ms).expect("ordered entry is accepted")
    }

    pub fn deliver_both(&self, first: &mut GridCoauthor, second: &mut GridCoauthor, entry: &OrderedEntry, now_ms: u64) -> (Step, Step) {
        let first_step = self.deliver(first, entry, now_ms);
        let second_step = self.deliver(second, entry, now_ms.saturating_add(1));
        (first_step, second_step)
    }

    pub fn head(&self) -> Seq {
        Seq(self.seq)
    }
}

// ---------------------------------------------------------------- checkpoint fidelity (G4 task 12)

/// The stored revision named for the checkpoint base in [`CheckpointRun`].
pub const CHECKPOINT_REVISION: &str = "r-checkpoint";

/// Sends a write (`write`) or verify ticket at `at` and returns the client's answer.
pub fn ticket_answer(client: &mut GridCoauthor, write: bool, at: Seq, ticket: u64, now_ms: u64) -> TicketAnswer {
    let frame = json!({
        "type":"checkpoint_ticket",
        "ticket":format!("00000000-0000-0000-0000-{ticket:012x}"),
        "purpose":if write { "write" } else { "verify" },
        "at":at.0,
        "base":CHECKPOINT_REVISION,
        "major":null
    });
    let step = client.receive(&frame.to_string(), now_ms).expect("the ticket is accepted");
    let event = step
        .events
        .iter()
        .find(|event| matches!(event, Event::CheckpointRequested { .. } | Event::VerifyRequested { .. }))
        .expect("the ticket reaches the host");
    client.answer_ticket(event).expect("the host answers the ticket")
}

/// The checkpoint a write ticket produced.
pub fn written(answer: TicketAnswer) -> GridCheckpoint {
    match answer {
        TicketAnswer::Write { checkpoint, .. } => checkpoint,
        _ => panic!("a write ticket writes a checkpoint"),
    }
}

/// What a verify ticket reported: the checkpoint file's hash and its identity data's hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verified {
    pub sha256: [u8; 32],
    pub session_sha256: [u8; 32],
}

/// The hashes a verify ticket reported.
pub fn verified(answer: TicketAnswer) -> Verified {
    match answer {
        TicketAnswer::Verify { sha256, session_sha256, .. } => Verified { sha256, session_sha256 },
        _ => panic!("a verify ticket reports a hash"),
    }
}

/// The SHA-256 of a written checkpoint's identity data, as the room hashes the upload.
pub fn session_sha256(checkpoint: &GridCheckpoint) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(&checkpoint.session_data).into()
}

/// Joins `client` as `participant` on the checkpoint base at `at` (loaded with its identity data
/// under `stamp`, the stamp of the entry at `at`), then catches up on the retained entries.
pub fn join_from_checkpoint(
    client: &mut GridCoauthor,
    participant: u32,
    base: &GridCheckpoint,
    at: Seq,
    stamp: Stamp,
    retained: &[OrderedEntry],
    now_ms: u64,
) -> ClientPlace {
    let id = client
        .tick(now_ms)
        .into_iter()
        .find_map(|frame| match serde_json::from_str::<LiveClientMsg>(&frame).expect("client frame is valid") {
            LiveClientMsg::LiveJoin { client, .. } => Some(client),
            _ => None,
        })
        .expect("a new co-author sends a live join");
    let joined = json!({
        "type":"live_joined",
        "session":"00000000-0000-0000-0000-00000000005e",
        "participant":participant,
        "place":{"is":"new"},
        "can_edit":true,
        "heartbeat_ms":60_000,
        "people":[]
    });
    client.receive(&joined.to_string(), now_ms.saturating_add(1)).expect("join is accepted");
    let head = retained.last().map_or(at, |entry| entry.seq);
    let catchup = json!({"type":"catchup","base":at.0,"revision":CHECKPOINT_REVISION,"from":at.0,"head":head.0});
    let step = client.receive(&catchup.to_string(), now_ms.saturating_add(2)).expect("catch-up is accepted");
    assert!(step.events.iter().any(|event| matches!(event, Event::LoadBase { at: base_at, .. } if *base_at == at)), "the checkpoint is the base");
    client
        .load_base(LiveBase::Checkpoint { bytes: &base.bytes, session_data: &base.session_data, stamp, compact: false })
        .expect("the checkpoint and its identity data load");
    assert_eq!(client.editor().agreement_digest(), base.entered, "the loaded base is the checkpoint's entered content");
    client.base_ready(now_ms.saturating_add(3)).expect("the loaded base is accepted");
    for (i, entry) in retained.iter().enumerate() {
        client.receive(&entry.frame, now_ms.saturating_add(4).saturating_add(i as u64)).expect("a retained entry applies");
    }
    assert_eq!(client.phase(), Phase::Live, "the catch-up ends live");
    ClientPlace { client: id, participant: ParticipantNo(participant) }
}

/// What [`checkpoint_run`] leaves: the base a write ticket stored while a change was waiting,
/// the entries retained after it, and the checkpoints written at the end.
pub struct CheckpointRun {
    /// Bob's checkpoint at `base_at`, written while his own change was still waiting.
    pub base: GridCheckpoint,
    pub base_at: Seq,
    /// The stamp of the entry at `base_at`, under which the base loads.
    pub base_stamp: Stamp,
    /// The retained entries after the base, in order.
    pub suffix: Vec<OrderedEntry>,
    pub head: Seq,
    /// Alice's checkpoint at `head`: she applied every entry from the start.
    pub straight: GridCheckpoint,
    /// Carol's checkpoint at `head`: she joined late on `base` and caught up on the suffix.
    pub late: GridCheckpoint,
    /// Bob's verify hashes at `head`.
    pub bob_verify: Verified,
}

/// Orders one proposal, delivers it to every client and checks that every client with nothing
/// waiting shows the same entered content.
fn order_everywhere(
    server: &mut DeterministicServer,
    log: &mut Vec<OrderedEntry>,
    clients: &mut [GridCoauthor],
    proposed: &ProposedEntry,
    now_ms: u64,
) {
    let n = log.len() as u32;
    let entry = server.order(proposed, 1_795_219_200_000 + i64::from(n) * 61_000, 101 + 7 * n);
    for (i, client) in clients.iter_mut().enumerate() {
        let step = client.receive(&entry.frame, now_ms.saturating_add(i as u64)).expect("ordered entry is accepted");
        assert!(
            !step.events.iter().any(|event| matches!(event, Event::Applied { effect: collab_live::client::Effect::NoEffect { .. }, .. })),
            "entry {} did nothing on client {i}: {step:?}",
            entry.seq.0
        );
    }
    let settled: Vec<(usize, [u8; 32])> =
        clients.iter().enumerate().filter(|(_, client)| client.waiting() == 0).map(|(i, client)| (i, client.editor().agreement_digest())).collect();
    for (i, digest) in &settled[1..] {
        assert_eq!(*digest, settled[0].1, "client {i} differs from client {} in entered content after entry {}", settled[0].0, entry.seq.0);
    }
    log.push(entry);
}

#[allow(clippy::too_many_arguments)]
fn share(
    server: &mut DeterministicServer,
    log: &mut Vec<OrderedEntry>,
    clients: &mut [GridCoauthor],
    places: &[ClientPlace],
    author: usize,
    id: &str,
    params: Json,
    now_ms: u64,
) {
    let step = clients[author].submit(id, params, 2026, now_ms).expect("shared command submits");
    let proposed = server.one_proposal(&places[author], &step);
    order_everywhere(server, log, clients, &proposed, now_ms.saturating_add(1));
}

fn share_undo(
    server: &mut DeterministicServer,
    log: &mut Vec<OrderedEntry>,
    clients: &mut [GridCoauthor],
    places: &[ClientPlace],
    author: usize,
    now_ms: u64,
) {
    let step = clients[author].undo(now_ms).expect("personal undo submits");
    let proposed = server.one_proposal(&places[author], &step);
    order_everywhere(server, log, clients, &proposed, now_ms.saturating_add(1));
}

/// Two GridCraft clients edit. Bob is given a write ticket while his own change waits and Alice
/// verifies it. Carol joins late on that checkpoint, with its identity data, and catches up on
/// the retained entries. Then all three make further content and structural changes, including
/// Alice undoing a row deletion made before the checkpoint. Entered content is compared after
/// every entry. At the end Alice (straight through) and Carol (checkpoint plus suffix) both write
/// the head and Bob verifies it.
pub fn checkpoint_run() -> CheckpointRun {
    let editor = |names: &[&str]| {
        GridCoauthor::new(
            LiveConfig { format: GRIDCRAFT_LIVE_FORMAT.to_string(), crashed_at: None },
            GridLiveEditor::new(session_with_sheets(names)).expect("editor builds"),
        )
    };
    let mut server = DeterministicServer::new();
    let mut clients = vec![editor(&["Data", "Notes"]), editor(&["Data", "Notes"])];
    let mut places = Vec::new();
    places.push(server.join(&mut clients[0], 10));
    places.push(server.join(&mut clients[1], 20));
    let (alice, bob, carol) = (0, 1, 2);
    let mut log = Vec::new();

    let before: [(usize, &str, Json); 9] = [
        (alice, "cell.set", json!({"sheet":"Data","cell":"A1","input":"5"})),
        (bob, "cell.set", json!({"sheet":"Data","cell":"A2","input":"=A1*2"})),
        (alice, "cell.set", json!({"sheet":"Data","cell":"A3","input":"=NOW()"})),
        (bob, "cell.set", json!({"sheet":"Data","cell":"A4","input":"=RAND()"})),
        (alice, "home.insertRows", json!({"sheet":"Data","rows":"2:2"})),
        (bob, "home.bold", json!({"sheet":"Data","range":"A1:A3","on":true})),
        // Alice deletes the RAND row; only she may revive it.
        (alice, "home.deleteRows", json!({"sheet":"Data","rows":"5:5"})),
        (bob, "home.insertSheet", json!({"name":"Added"})),
        (alice, "cell.set", json!({"sheet":"Notes","cell":"B2","input":"=Data!A3+1"})),
    ];
    for (i, (author, id, params)) in before.into_iter().enumerate() {
        share(&mut server, &mut log, &mut clients, &places, author, id, params, 100 + 10 * i as u64);
    }

    // Bob's change waits while he writes the checkpoint and Alice verifies it.
    let waiting = clients[bob].submit("cell.set", json!({"sheet":"Data","cell":"C1","input":"waiting"}), 2026, 200).expect("waiting edit submits");
    let waiting = server.one_proposal(&places[bob], &waiting);
    let base_at = server.head();
    let base = written(ticket_answer(&mut clients[bob], true, base_at, 1, 210));
    let alice_hash = verified(ticket_answer(&mut clients[alice], false, base_at, 2, 211));
    assert_eq!(base.sha256, alice_hash.sha256, "the writer with a waiting change and the verifier write the same bytes");
    assert_eq!(session_sha256(&base), alice_hash.session_sha256, "and the same identity data");
    assert_eq!(base.entered, clients[alice].editor().agreement_digest(), "the base is the agreed state");
    assert_ne!(base.entered, clients[bob].editor().agreement_digest(), "Bob still shows his waiting change");
    let last = log.last().expect("entries before the checkpoint");
    let base_stamp = Stamp { at_ms: last.at_ms, seed: last.seed };

    // Alice's change is ordered before Bob's waiting one, which is re-run on top of it.
    share(&mut server, &mut log, &mut clients, &places, alice, "cell.set", json!({"sheet":"Data","cell":"B1","input":"after the ticket"}), 220);
    order_everywhere(&mut server, &mut log, &mut clients, &waiting, 230);
    assert_eq!(clients[bob].waiting(), 0, "Bob's change is agreed");

    // Carol joins late on Bob's checkpoint and the retained entries.
    let mut late = editor(&["Placeholder"]);
    let retained: Vec<OrderedEntry> = log.iter().filter(|entry| entry.seq > base_at).cloned().collect();
    places.push(join_from_checkpoint(&mut late, 3, &base, base_at, base_stamp, &retained, 300));
    clients.push(late);
    assert_eq!(clients[carol].editor().agreement_digest(), clients[alice].editor().agreement_digest(), "the late joiner agrees on arrival");

    share(&mut server, &mut log, &mut clients, &places, carol, "home.insertColumns", json!({"sheet":"Data","cols":"B:B"}), 400);
    // Alice undoes her edit after the ticket, her Notes formula, then her row deletion from
    // before the checkpoint: Carol knows the row is Alice's from the checkpoint's identity data.
    for i in 0..3 {
        share_undo(&mut server, &mut log, &mut clients, &places, alice, 410 + 10 * i);
    }
    let revived = {
        let workbook = &clients[carol].editor().session().doc().expect("document exists").wb;
        workbook
            .sheet(0)
            .expect("Data exists")
            .cell(gridcraft_core::CellRef::new(4, 0))
            .and_then(|cell| cell.formula.as_ref())
            .map(|formula| formula.text.clone())
    };
    assert_eq!(revived.as_deref(), Some("RAND()"), "the late joiner revives Alice's deleted row");
    share(&mut server, &mut log, &mut clients, &places, bob, "sheet.rename", json!({"sheet":"Notes","name":"Summary"}), 450);
    share(&mut server, &mut log, &mut clients, &places, carol, "cell.set", json!({"sheet":"Data","cell":"A7","input":"late"}), 460);
    share(&mut server, &mut log, &mut clients, &places, carol, "home.deleteRows", json!({"sheet":"Data","rows":"3:3"}), 470);
    share(&mut server, &mut log, &mut clients, &places, bob, "home.deleteColumns", json!({"sheet":"Data","cols":"D:D"}), 480);
    share(&mut server, &mut log, &mut clients, &places, alice, "cell.set", json!({"sheet":"Summary","cell":"C3","input":"=SUM(Data!A1:A9)"}), 490);

    let head = server.head();
    let straight = written(ticket_answer(&mut clients[alice], true, head, 3, 500));
    let late = written(ticket_answer(&mut clients[carol], true, head, 4, 501));
    let bob_verify = verified(ticket_answer(&mut clients[bob], false, head, 5, 502));
    let suffix = log.into_iter().filter(|entry| entry.seq > base_at).collect();
    CheckpointRun { base, base_at, base_stamp, suffix, head, straight, late, bob_verify }
}

// ------------------------------------------------- fragmented identity axes (G5 task 8, I7)

/// The live and deleted runs of one sheet's row axis.
pub fn row_axis_runs(ids: &WorkbookIds, sheet: LiveId) -> usize {
    let rows = serde_json::to_value(&ids.by_id.get(&sheet).expect("the sheet has identities").rows).expect("the row axis serialises");
    let count = |part: &str| rows[part].as_array().map_or(0, Vec::len);
    count("live") + count("deleted")
}

/// The participant the fixture's deletions are owned by: someone who has left, so no test
/// client can revive them.
pub const FRAGMENTED_OWNER: u32 = 99;

/// Identity data for `session`'s workbook whose first sheet's row axis holds `2 * pairs + 1`
/// runs: `pairs` single live rows, each followed by one deleted row, then one long run. That is
/// the axis `pairs` separate one-row deletions leave (here the fillers they appended carry on
/// the long run), with each deletion owned by [`FRAGMENTED_OWNER`] as a checkpoint written
/// after live deletions records it. Making those deletions one entry at a time would take
/// minutes in a debug build.
pub fn fragmented_session_data(session: &Session, pairs: u32) -> Vec<u8> {
    let ids = WorkbookIds::from_workbook(&session.doc().expect("a document").wb).expect("base identities build");
    let mut data: Json = serde_json::from_slice(&ids.session_data().expect("base identity data")).expect("identity data is JSON");
    let mut live = Vec::with_capacity(pairs as usize + 1);
    let mut deleted = Vec::with_capacity(pairs as usize);
    let mut owners = Vec::with_capacity(pairs as usize);
    let sheet = data["by_id"][0]["key"].clone();
    for i in 0..pairs {
        live.push(json!({"participant":0,"start":2 * i,"len":1}));
        deleted.push(json!({"run":{"participant":0,"start":2 * i + 1,"len":1},"was_at":i + 1,"after":[0, 2 * i]}));
        owners.push(json!({"sheet":sheet,"rows":true,"first":[0, 2 * i + 1],"count":1,"by":FRAGMENTED_OWNER}));
    }
    data["deleted_by"] = Json::Array(owners);
    live.push(json!({"participant":0,"start":2 * pairs,"len":gridcraft_core::MAX_ROWS - pairs}));
    data["by_id"][0]["value"]["rows"] = json!({"live":live,"deleted":deleted});
    serde_json::to_vec(&data).expect("identity data serialises")
}

/// A co-author on one sheet, `Data`, whose row axis is fragmented as
/// [`fragmented_session_data`] describes (`pairs` 0 is an untouched axis).
pub fn fragmented_coauthor(pairs: u32) -> GridCoauthor {
    let session = session_with_sheets(&["Data"]);
    let data = fragmented_session_data(&session, pairs);
    let editor = GridLiveEditor::from_session_data(session, &data).expect("fragmented identity data loads");
    GridCoauthor::new(LiveConfig { format: GRIDCRAFT_LIVE_FORMAT.to_string(), crashed_at: None }, editor)
}

/// A `home.deleteRows` entry naming `rows`, made by hand as an older or hostile sender could:
/// it skips the sender's own fragmentation check.
pub fn delete_rows_payload(action: u64, sheet: LiveId, rows: Vec<LiveId>, first: LiveId) -> String {
    let entry = SharedEntry { version: 1, action, command: SharedCommand::DeleteRows { sheet, rows: IdentityDelete { ids: rows, first } } };
    serde_json::to_string(&entry).expect("the entry serialises")
}

/// What [`fragmented_cell_run`] measured.
pub struct FragmentedRun {
    /// Live and deleted runs of the row axis.
    pub runs: usize,
    /// Owned deleted runs in the identity data (each a `deleted_by` entry).
    pub owners: usize,
    /// Identity runs copied or walked during the entries (`identity_runs_visited`).
    pub runs_visited: u64,
    /// Each entry's time from submit until both clients applied it, in milliseconds.
    pub entry_ms: Vec<f64>,
    /// Both clients' entered digests at the end.
    pub entered: [[u8; 32]; 2],
}

/// Alice and Bob, on an axis of `2 * pairs + 1` runs, make `entries` `cell.set` entries in
/// turn, spread over the first 60,000 rows; each is ordered and applied by both. `now_ms` is the
/// clock the per-entry times are read from (a run that only counts can pass a constant).
pub fn fragmented_cell_run(pairs: u32, entries: u32, now_ms: &mut dyn FnMut() -> f64) -> FragmentedRun {
    let mut server = DeterministicServer::new();
    let mut alice = fragmented_coauthor(pairs);
    let mut bob = fragmented_coauthor(pairs);
    let places = [server.join(&mut alice, 10), server.join(&mut bob, 20)];
    let sheet = alice.editor().ids().sheet_id(0).expect("the sheet has an identity");
    let runs = row_axis_runs(alice.editor().ids(), sheet);
    let data: Json = serde_json::from_slice(&alice.editor().ids().session_data().expect("identity data")).expect("identity data is JSON");
    let owners = data["deleted_by"].as_array().map_or(0, Vec::len);
    let mut entry_ms = Vec::with_capacity(entries as usize);
    let visited = identity_runs_visited();
    for i in 0..entries {
        let column = char::from(b'A' + (i % 26) as u8);
        let cell = format!("{column}{}", 1 + (u64::from(i) * 7_919) % 60_000);
        // Five entries a second, inside the client's pace limit.
        let clock = 100 + u64::from(i) * 200;
        let params = json!({"sheet":0,"cell":cell,"input":format!("v{i}")});
        let started = now_ms();
        let author = if i % 2 == 0 { &mut alice } else { &mut bob };
        let step = author.submit("cell.set", params, 2026, clock).expect("the edit is made");
        let proposed = server.one_proposal(&places[(i % 2) as usize], &step);
        let ordered = server.order(&proposed, 1_795_219_200_000 + i64::from(i), i);
        server.deliver_both(&mut alice, &mut bob, &ordered, clock + 1);
        entry_ms.push(now_ms() - started);
    }
    let runs_visited = identity_runs_visited() - visited;
    let entered = [alice.editor().agreement_digest(), bob.editor().agreement_digest()];
    FragmentedRun { runs, owners, runs_visited, entry_ms, entered }
}
