use collab_core::ids::Seq;
use collab_live::client::{EntryView, Event, LiveClient, LiveConfig, Stamp};
use collab_live::ids::LiveId;
use gridcraft_engine::coauthor::{GRIDCRAFT_LIVE_FORMAT, GridLiveEditor, PreparedChange};
use gridcraft_engine::{DocState, Session, Trust};
use gridcraft_model::{Sheet, Workbook};
use protocol::live::ParticipantNo;
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
