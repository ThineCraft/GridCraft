use super::{Json, Notice, NoticeKind, OpenWindow, SheetApp, clean_error, json};

impl SheetApp {
    pub fn open_path(&mut self, path: &str) {
        match self.run("file.open", json!({"path": path})) {
            Ok(r) => self.after_open(Some(path), &r),
            // A password-protected workbook: ask for the password; the dialog opens it.
            Err(e) if e.contains(gridcraft_engine::io::PASSWORD_REQUIRED) => self.open_dialog("openPassword", json!({"path": path})),
            Err(e) => self.message = Some(("GridCraft".into(), clean_error(&e))),
        }
        self.after_engine();
    }

    /// What follows a successful `file.open`: the workbook shows (not the Backstage), the Recent
    /// list (for a file on disk) and the open warnings notice. Shared by File › Open, the
    /// password dialog and the control channel.
    pub fn after_open(&mut self, path: Option<&str>, r: &Json) {
        self.backstage = None;
        if let Some(path) = path {
            self.ui.recent.retain(|p| p != path);
            self.ui.recent.insert(0, path.to_string());
            self.ui.recent.truncate(20);
        }
        self.show_open_warnings(r);
    }

    /// Turns the `warnings` a `file.open` result carries into an info-bar notice, choosing the
    /// most important one (a preserved VBA project outranks, say, a skipped optional part).
    /// Kept deliberately generic: any open-time warning can reach the user this way.
    pub fn show_open_warnings(&mut self, r: &Json) {
        // The opened workbook's window shows them, however it was opened (a script may have run
        // `file.open` on the session directly).
        self.follow_active_window();
        let Some(list) = r.get("warnings").and_then(Json::as_array) else {
            self.notice = None;
            return;
        };
        let msgs: Vec<&str> = list.iter().filter_map(Json::as_str).collect();
        if msgs.is_empty() {
            // A clean open clears any notice left over from the previous document.
            self.notice = None;
            return;
        }
        let vba: Vec<&str> = msgs.iter().copied().filter(|m| m.to_lowercase().contains("macro") || m.contains("VBA")).collect();
        if !vba.is_empty() {
            // The reader says "dropped" when the VBA project was missing or unreadable; never promise to keep it then.
            let dropped = vba.iter().any(|m| m.contains("dropped"));
            let text = if dropped {
                "This workbook's VBA macros could not be loaded and were dropped, so they will not be in the saved file. Keep the original file."
            } else {
                "This workbook contains VBA macros. GridCraft keeps them when you save as .xlsm, but does not run them."
            };
            self.notice = Some(Notice { text: text.into(), kind: NoticeKind::Warning, action: Some(("What this means".into(), "macros".into())) });
        } else {
            let text = if msgs.len() == 1 {
                msgs[0].to_string()
            } else {
                format!("{} items in this file were handled differently. First: {}", msgs.len(), msgs[0])
            };
            self.notice = Some(Notice { text, kind: NoticeKind::Info, action: None });
        }
    }

    /// The open windows in order, for the Switch Windows lists.
    pub fn open_windows(&self) -> Vec<OpenWindow> {
        let active = self.session.active_window();
        self.session
            .windows()
            .iter()
            .filter_map(|w| {
                let d = self.session.documents().iter().find(|d| d.uid == w.doc_uid)?;
                let title = self.session.window_title(w.id)?;
                Some(OpenWindow { window: w.id, doc: d.uid, title, dirty: d.is_dirty(), active: active == Some(w.id) })
            })
            .collect()
    }

    /// Opens files the person delivered asynchronously (web file picker, dropped files). While the
    /// files are locked `run` refuses them, quietly: only the host's workbooks
    /// ([`Self::poll_host_opens`]) get past the lock.
    pub(super) fn poll_async_opens(&mut self) {
        for (name, bytes) in take_queue(self.services.inbox.as_ref()) {
            self.open_arrived(&name, &bytes, false);
        }
    }

    /// Opens the workbooks a host page sent to edit ([`crate::Services::host_open`]). Unlocked they
    /// open as a picked file does. While the files are locked each gets past the lock and ends as
    /// the one workbook shown: the others close, edited or not, without asking (what was typed
    /// before the host's file arrived isn't the host's). A protected one asks for its password
    /// first, and the same happens when it opens.
    pub(super) fn poll_host_opens(&mut self) {
        for (name, bytes) in take_queue(self.services.host_open.as_ref()) {
            self.open_arrived(&name, &bytes, true);
        }
    }

    fn open_arrived(&mut self, name: &str, bytes: &[u8], host: bool) {
        let b64 = gridcraft_engine::io::base64_encode(bytes);
        let params = json!({"name": name, "base64": &b64});
        let locked_host = host && self.files_locked;
        let r = if locked_host { self.run_command("file.open", params) } else { self.run("file.open", params) };
        match r {
            // A file picked from File › Open (web) arrives here: show it, not the Backstage.
            Ok(r) => {
                if locked_host {
                    self.keep_only_active_workbook();
                }
                self.backstage = None;
                self.show_open_warnings(&r);
            }
            // The person's workbook on a locked page: refused without a message.
            Err(e) if e == super::commands::FILES_LOCKED => log::info!("{name} not opened: the open workbook is locked"),
            Err(e) if e.contains(gridcraft_engine::io::PASSWORD_REQUIRED) => {
                self.backstage = None;
                let mut params = json!({"name": name, "base64": b64});
                if locked_host {
                    // Lets the password dialog's open past the lock (`run_dialog_command`).
                    params[HOST_OPEN] = json!(true);
                }
                self.open_dialog("openPassword", params);
            }
            Err(e) => self.message = Some(("GridCraft".into(), clean_error(&e))),
        }
        self.after_engine();
    }

    /// Closes every workbook but the active one, unsaved changes and all: a locked page shows
    /// only the host's workbook.
    pub(super) fn keep_only_active_workbook(&mut self) {
        let Some(keep) = self.session.active().map(|d| d.uid) else { return };
        while let Some(i) = self.session.documents().iter().position(|d| d.uid != keep) {
            self.session.close_document(i);
        }
        self.after_engine();
    }

    /// AutoSave: save workbooks that have a file shortly after they change. This decision lives in
    /// `logic` so a hidden or logic-only frame still saves; the monotonic timestamp check keeps a
    /// forced hidden UI pass from saving the same change twice.
    pub(super) fn autosave(&mut self, ctx: &egui::Context) {
        let dirty_xlsx = self.session.active().is_some_and(|d| d.is_dirty() && d.path.as_deref().is_some_and(|p| p.ends_with(".xlsx")));
        let now = crate::monotonic_ms();
        let autosave_due =
            self.ui.autosave && self.editor.is_none() && self.text_box_editor.is_none() && now - self.grid.last_autosave > 2000.0 && dirty_xlsx;
        if autosave_due {
            self.grid.last_autosave = now;
            self.store_view_state();
            let _ = self.session.run("file.save", json!({}));
        } else if self.ui.autosave && dirty_xlsx {
            // Keep checking even when no input arrives while the window is hidden.
            ctx.request_repaint_after(std::time::Duration::from_secs(2));
        }
    }

    /// Flip AutoSave (title-bar switch / Options ▸ Save). Persisted with the other UI
    /// preferences, so it stays as the user left it across restarts (#1901).
    pub fn toggle_autosave(&mut self) {
        self.ui.autosave = !self.ui.autosave;
    }
}

/// The password dialog's mark for a host's workbook ([`SheetApp::poll_host_opens`]); removed
/// before `file.open` runs.
pub(super) const HOST_OPEN: &str = "hostOpen";

fn take_queue(queue: Option<&crate::Inbox>) -> Vec<(String, Vec<u8>)> {
    queue.map(|q| std::mem::take(&mut *q.lock().unwrap_or_else(std::sync::PoisonError::into_inner))).unwrap_or_default()
}
