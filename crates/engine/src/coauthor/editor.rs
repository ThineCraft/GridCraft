use std::sync::Arc;

use collab_core::ids::Seq;
use collab_live::client::{Crash, Effect, EntryView, LiveEditor, Outcome, Stamp};

use super::{WorkbookIds, decode_entry, resolve_command};
use crate::fingerprint::{DocId, fingerprint};
use crate::{Calc, EngineError, Session};

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

/// GridCraft's document adapter for the shared live-client apply loop.
pub struct GridLiveEditor {
    session: Session,
    ids: WorkbookIds,
    applying_seq: Option<Seq>,
}

impl GridLiveEditor {
    pub fn new(session: Session) -> Result<Self, super::CoauthorError> {
        let ids = WorkbookIds::from_workbook(&session.doc()?.wb)?;
        Ok(Self { session, ids, applying_seq: None })
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

    fn no_effect(reason: &str, read_stamp: bool) -> Outcome {
        Outcome { effect: Effect::NoEffect { reason: reason.to_string() }, read_stamp }
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
        if let Ok(document) = self.session.doc_mut() {
            document.wb = snapshot.document.wb.clone();
            document.calc = snapshot.document.calc.clone();
            document.revision = snapshot.document.revision;
            self.ids = snapshot.ids.clone();
        }
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
        let before = self.snapshot();
        if let Ok(document) = self.session.doc_mut() {
            document.calc.clear_stamp_read();
        }
        if let Some(stamp) = entry.stamp {
            self.apply_stamp(stamp);
        }
        let result = self.session.execute(&resolved.id, resolved.params);
        let read_stamp = entry.stamp.is_none() && self.session.doc().is_ok_and(|document| document.calc.stamp_was_read());
        if let Err(error) = result {
            self.restore(&before);
            return match error {
                EngineError::Internal(_, _) => Err(Crash),
                _ => Ok(Self::no_effect("The shared change could not be applied.", read_stamp)),
            };
        }
        if let Some(structural) = resolved.structural
            && self.ids.apply_structural(&structural).is_err()
        {
            self.restore(&before);
            return Ok(Self::no_effect("The shared identities could not be updated.", read_stamp));
        }
        Ok(Outcome { effect: Effect::Applied, read_stamp })
    }

    fn note_applying(&mut self, seq: Option<Seq>) {
        self.applying_seq = seq;
    }
}
