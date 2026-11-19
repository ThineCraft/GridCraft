use collab_core::ids::{ClientId, Seq};
use collab_live::client::{EntryView, Event, LiveClient, LiveConfig, Stamp, Step};
use collab_live::ids::LiveId;
use gridcraft_engine::coauthor::{GRIDCRAFT_LIVE_FORMAT, GridCoauthor, GridLiveEditor, PreparedChange};
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
            let first = LiveId(7, *next);
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
