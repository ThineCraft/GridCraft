//! Pure host-mode rules, shared by the bridge ([`crate::host`]) and testable without a browser
//! (like [`crate::prefs`]): the name the bridge fills in when a host's open request has none.
//! Which pages host the app is `craft-web-host`'s.

/// The file name used when an open request carries no usable one.
pub const DEFAULT_OPEN_NAME: &str = "Book.xlsx";

/// The name `craft-web-host` gives an open request that carries no usable one (it cleans a host's
/// name and falls back to this).
const NAMELESS: &str = "document";

/// A usable file name for an open request: the cleaned name the crate hands over, or
/// [`DEFAULT_OPEN_NAME`] when the host sent none.
pub fn open_name(name: &str) -> String {
    if name == NAMELESS { DEFAULT_OPEN_NAME.to_string() } else { name.to_string() }
}

/// Saved files that count as a workbook: the document is then clean under the name it was saved as.
pub const WORKBOOK_EXTS: &[&str] = &["xlsx"];

pub fn is_workbook(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| WORKBOOK_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// Whether a file GridCraft saves goes to the host page. In a live session the room keeps the
/// document from its checkpoints, so the workbook's own Save is not handed over; exports are.
pub fn hands_save_to_page(name: &str, live: bool) -> bool {
    !(live && is_workbook(name))
}

/// The `dirty` state the page hears: never dirty while live, since nothing is saved by the editor.
pub fn reported_dirty(live: bool, any_dirty: bool) -> bool {
    !live && any_dirty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_names_fall_back_to_the_default_when_the_host_sent_none() {
        assert_eq!(open_name("Budget.xlsx"), "Budget.xlsx");
        assert_eq!(open_name("Sheet2!Book.xlsx"), "Sheet2!Book.xlsx");
        assert_eq!(open_name("document"), DEFAULT_OPEN_NAME, "what the crate says for a missing or blank name");
    }

    #[test]
    fn a_live_workbook_is_not_handed_to_the_page_on_save() {
        assert!(hands_save_to_page("Book.xlsx", false), "an ordinary hosted save goes to the page");
        assert!(!hands_save_to_page("Book.xlsx", true), "the room keeps a live document");
        assert!(!hands_save_to_page("Book.XLSX", true));
        assert!(hands_save_to_page("Book.pdf", true), "an export still goes to the page");
        assert!(!reported_dirty(true, true), "a live document is never dirty for the page");
        assert!(reported_dirty(false, true));
        assert!(!reported_dirty(false, false));
    }
}
