//! WinSCP-style remote file browser: opens its own SFTP subsystem using the
//! saved SSH credentials and renders a navigable file table with upload,
//! download, rename, delete and folder creation. Original UI (egui), no
//! external file-manager code or assets.

use crate::log::{push_log, LogEntry, LogLevel, SharedLogs};
use crate::session::SshSession;
use crate::ssh;
use crate::theme;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Frame, Margin, RichText, Sense, Stroke, Ui,
};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};


// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct FbEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: u32,
}

#[derive(Clone, Debug)]
pub enum FbStatus {
    Connecting,
    Connected,
    Failed(String),
}

enum FbCmd {
    Navigate(String),
    Up,
    Refresh,
    MkDir(String),
    Remove { path: String, is_dir: bool },
    Rename { from: String, to: String },
    Download { remote: String, local: String },
    Upload { remote: String, local: String },
    Close,
}

enum FbEvent {
    Connected { cwd: String, entries: Vec<FbEntry> },
    Listing { cwd: String, entries: Vec<FbEntry> },
    Done(String),
    OpFailed(String),
    Failed(String),
}

// ---------------------------------------------------------------------------
// Local editors (like WinSCP's editor menu): download → open → re-upload
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditorCfg {
    pub name: String,
    pub path: String,
}

pub type SharedEditors = Arc<RwLock<Vec<EditorCfg>>>;

fn editors_json_path() -> std::path::PathBuf {
    let mut p = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    p = p.join("culvert");
    p.join("editors.json")
}

fn save_editors(list: &[EditorCfg]) {
    let path = editors_json_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(list) {
        let _ = std::fs::write(&path, json);
    }
}

/// Detect a handful of well-known editors for the first-run default list.
/// Everything else is user-added with an arbitrary executable path.
fn default_editors() -> Vec<EditorCfg> {
    let mut out = Vec::new();
    let notepad = std::path::PathBuf::from(r"C:\Windows\notepad.exe");
    if notepad.exists() {
        out.push(EditorCfg {
            name: "Notepad".into(),
            path: notepad.display().to_string(),
        });
    }
    // First VS Code found wins: system install, then per-user install.
    let code_paths = [
        r"C:\Program Files\Microsoft VS Code\Code.exe",
        &format!(
            "{}\\Programs\\Microsoft VS Code\\Code.exe",
            std::env::var_os("LOCALAPPDATA")
                .map(|s| s.display().to_string())
                .unwrap_or_default()
        ),
    ];
    for path in code_paths {
        if std::path::Path::new(path).exists() {
            out.push(EditorCfg {
                name: "VS Code".into(),
                path: path.to_string(),
            });
            break;
        }
    }
    let npp = r"C:\Program Files\Notepad++\notepad++.exe";
    if std::path::Path::new(npp).exists() {
        out.push(EditorCfg {
            name: "Notepad++".into(),
            path: npp.to_string(),
        });
    }
    out
}

/// Load the user's editor list once at startup.
pub fn load_editors() -> SharedEditors {
    let path = editors_json_path();
    let list = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Vec<EditorCfg>>(&raw).ok())
        .unwrap_or_else(default_editors);
    Arc::new(RwLock::new(list))
}

/// An in-flight "download, open in editor, re-upload on close" job.
struct EditJob {
    /// Base file name, for the status note.
    file: String,
    editor: String,
    /// Remote absolute path to re-upload to.
    remote: String,
    local: std::path::PathBuf,
    child: std::process::Child,
}

// ---------------------------------------------------------------------------
// Tab session
// ---------------------------------------------------------------------------

pub struct FileBrowser {
    pub title: String,
    pub status: FbStatus,
    entries: Vec<FbEntry>,
    cwd: String,
    path_edit: String,
    selected: Option<String>,
    /// Operation in flight; shows a busy note and gates repeat actions.
    busy: Option<String>,
    last_status: Option<(bool, String)>,
    /// "New folder" modal open + typed name.
    folder_modal: bool,
    folder_name: String,
    /// (cwd, old name); the rename text lives in `rename_value`.
    rename_modal: Option<(String, String)>,
    rename_value: String,
    /// Set by the error overlay; app.rs closes the tab after drawing.
    pub want_close: bool,
    /// User-configured local editors (shared across all browsers/tabs).
    editors: SharedEditors,
    /// "Local editors" settings modal + its input fields / active slot.
    editors_modal: bool,
    ed_name: String,
    ed_path: String,
    /// Download that precedes an editor launch: (remote, local, editor name/path).
    pending_edit: Option<(String, std::path::PathBuf, String, String)>,
    /// Editor is open on a local copy; re-uploaded when the process exits.
    edit_job: Option<EditJob>,
    cmd_tx: UnboundedSender<FbCmd>,
    evt_rx: UnboundedReceiver<FbEvent>,
    rt: tokio::runtime::Handle,
    session: SshSession,
    ctx: egui::Context,
    logs: SharedLogs,
}

impl FileBrowser {
    pub fn spawn(
        ctx: &egui::Context,
        rt: tokio::runtime::Handle,
        logs: SharedLogs,
        editors: SharedEditors,
        session: SshSession,
    ) -> Self {
        let (cmd_tx, _) = unbounded_channel::<FbCmd>();
        let (_, evt_rx) = unbounded_channel::<FbEvent>();
        let mut me = Self {
            title: session.name.clone(),
            status: FbStatus::Connecting,
            entries: Vec::new(),
            cwd: "/".to_string(),
            path_edit: String::new(),
            selected: None,
            busy: None,
            last_status: None,
            folder_modal: false,
            folder_name: String::new(),
            rename_modal: None,
            rename_value: String::new(),
            want_close: false,
            editors,
            editors_modal: false,
            ed_name: String::new(),
            ed_path: String::new(),
            pending_edit: None,
            edit_job: None,
            cmd_tx,
            evt_rx,
            rt,
            session,
            ctx: ctx.clone(),
            logs,
        };
        me.start_task();
        me
    }

    fn start_task(&mut self) {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let (evt_tx, evt_rx) = unbounded_channel();
        self.cmd_tx = cmd_tx;
        self.evt_rx = evt_rx;
        let session = self.session.clone();
        let logs = self.logs.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            sftp_task(session, logs, ctx, evt_tx, cmd_rx).await;
        });
    }

    pub fn reconnect(&mut self) {
        self.status = FbStatus::Connecting;
        self.entries.clear();
        self.selected = None;
        self.path_edit.clear();
        self.last_status = None;
        self.want_close = false;
        self.start_task();
    }

    pub fn close(&self) {
        let _ = self.cmd_tx.send(FbCmd::Close);
    }

    fn send(&self, cmd: FbCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    /// Drain async events. Returns true when the UI should repaint.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.evt_rx.try_recv() {
            match ev {
                FbEvent::Connected { cwd, entries } => {
                    self.status = FbStatus::Connected;
                    self.cwd = cwd.clone();
                    self.path_edit = cwd;
                    self.entries = entries;
                    self.selected = None;
                    changed = true;
                }
                FbEvent::Listing { cwd, entries } => {
                    self.cwd = cwd.clone();
                    self.path_edit = cwd;
                    self.entries = entries;
                    self.selected = None;
                    changed = true;
                }
                FbEvent::Done(msg) => {
                    self.busy = None;
                    // An edit-download finished: open the local copy in the
                    // chosen editor and start watching its process.
                    if let Some((remote, local, ed_name, ed_path)) = self.pending_edit.take() {
                        if msg.contains(local.display().to_string().as_str()) {
                            self.launch_editor(remote, local, ed_name, ed_path);
                        }
                    }
                    self.last_status = Some((true, msg));
                    changed = true;
                }
                FbEvent::OpFailed(msg) => {
                    self.busy = None;
                    self.last_status = Some((false, msg));
                    changed = true;
                }
                FbEvent::Failed(msg) => {
                    self.status = FbStatus::Failed(msg);
                    changed = true;
                }
            }
        }
        // Editor process ended → push the local copy back to the server
        // (WinSCP-style "edit and upload on close").
        if let Some(job) = &mut self.edit_job {
            if let Ok(Some(_)) = job.child.try_wait() {
                let (file, editor, remote, local) = (
                    job.file.clone(),
                    job.editor.clone(),
                    job.remote.clone(),
                    job.local.display().to_string(),
                );
                self.edit_job = None;
                self.send(FbCmd::Upload { remote, local });
                self.busy = Some(format!("Editing of {file} closed ({editor}) — re-uploading…"));
                changed = true;
            }
        }
        changed
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    pub fn ui(&mut self, ui: &mut Ui) {
        if !matches!(self.status, FbStatus::Connected) {
            self.lifecycle_overlay(ui);
            return;
        }

        // --- Toolbar -------------------------------------------------------
        Frame::NONE
            .fill(theme::BG_CARD())
            .corner_radius(CornerRadius::same(10))
            .stroke(Stroke::new(1.0_f32, theme::BORDER()))
            .inner_margin(Margin::symmetric(8, 5))
            .show(ui, |ui| {
                // horizontal_wrapped: in the narrow sidebar the trailing
                // buttons wrap to a second row instead of inflating the
                // panel's min-rect (which used to smear the layout).
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    ui.spacing_mut().item_spacing.y = 4.0;
                    if ui
                        .button(RichText::new("↑").size(12.0))
                        .on_hover_text("Parent folder")
                        .clicked()
                    {
                        self.send(FbCmd::Up);
                    }
                    if ui
                        .button(RichText::new("↻").size(12.0))
                        .on_hover_text("Refresh")
                        .clicked()
                    {
                        self.send(FbCmd::Refresh);
                    }
                    let path_resp = ui.add(
                        egui::TextEdit::singleline(&mut self.path_edit)
                            .desired_width(96.0f32.max(ui.available_width() - 250.0))
                            .font(FontId::monospace(11.5))
                            .margin(egui::vec2(6.0, 4.0)),
                    );
                    if path_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let p = self.path_edit.trim().to_string();
                        if !p.is_empty() {
                            self.send(FbCmd::Navigate(p));
                        }
                    }
                    if ui
                        .button(RichText::new("+ Folder").size(10.5))
                        .on_hover_text("Create folder in the current directory")
                        .clicked()
                    {
                        self.folder_modal = true;
                        self.folder_name.clear();
                    }
                    if ui
                        .button(RichText::new("✎").size(12.0))
                        .on_hover_text("Local editors…")
                        .clicked()
                    {
                        self.editors_modal = true;
                    }
                    if ui
                        .button(RichText::new("Upload").size(10.5))
                        .on_hover_text("Upload a local file into this directory")
                        .clicked()
                    {
                        if let Some(p) = rfd::FileDialog::new().pick_file() {
                            let name = p
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| "file.bin".to_string());
                            self.send(FbCmd::Upload {
                                remote: path_join(&self.cwd, &name),
                                local: p.display().to_string(),
                            });
                            self.busy = Some("Uploading…".to_string());
                        }
                    }
                    if self.selected.is_some() {
                        if ui
                            .button(RichText::new("Delete").size(10.5).color(theme::DANGER()))
                            .on_hover_text("Delete the selected entry")
                            .clicked()
                        {
                            if let Some(e) = self
                                .entries
                                .iter()
                                .find(|e| Some(&e.name) == self.selected.as_ref())
                                .cloned()
                            {
                                self.send(FbCmd::Remove {
                                    path: path_join(&self.cwd, &e.name),
                                    is_dir: e.is_dir,
                                });
                                self.busy = Some("Deleting…".to_string());
                                self.selected = None;
                            }
                        }
                    }
                });
            });
        ui.add_space(6.0);

        // --- Column header ---------------------------------------------------
        let hdr_rect = ui.available_rect_before_wrap();
        let hdr = egui::Rect::from_min_size(hdr_rect.min, egui::vec2(hdr_rect.width(), 16.0));
        ui.allocate_rect(hdr, Sense::hover());
        let size_x = hdr.left() + hdr.width() * 0.58;
        let p = ui.painter();
        let label = |x: f32, text: &str| {
            p.text(
                egui::pos2(x, hdr.center().y),
                Align2::LEFT_CENTER,
                text,
                FontId::proportional(9.5),
                theme::TEXT_FAINT(),
            );
        };
        // Narrow container (docked sidebar): size/modified don't fit — show
        // a single NAME column instead of clipped text at the panel edge.
        let narrow = hdr.width() < 280.0;
        label(hdr.left() + 20.0, "NAME");
        if !narrow {
            label(size_x, "SIZE");
            label(size_x + 90.0, "MODIFIED");
        }
        ui.add_space(1.0);

        // --- Rows ------------------------------------------------------------
        egui::ScrollArea::vertical().show(ui, |ui| {
            let entries = self.entries.clone();
            let cwd = self.cwd.clone();
            let editors = self
                .editors
                .read()
                .map(|v| v.clone())
                .unwrap_or_default();
            for entry in entries {
                let sel = self.selected.as_deref() == Some(entry.name.as_str());
                let action = file_row(ui, &entry, sel, &editors);
                match action {
                    Some(RowAction::Select) => self.selected = Some(entry.name.clone()),
                    Some(RowAction::Open) if entry.is_dir => {
                        self.send(FbCmd::Navigate(path_join(&cwd, &entry.name)));
                    }
                    Some(RowAction::Open) => {
                        self.start_download(&cwd, &entry.name, "Downloading…");
                    }
                    Some(RowAction::Download) => {
                        self.start_download(&cwd, &entry.name, "Downloading…");
                    }
                    Some(RowAction::Rename) => {
                        self.rename_modal = Some((cwd.clone(), entry.name.clone()));
                        self.rename_value = entry.name.clone();
                    }
                    Some(RowAction::Delete) => {
                        self.selected = None;
                        self.send(FbCmd::Remove {
                            path: path_join(&cwd, &entry.name),
                            is_dir: entry.is_dir,
                        });
                        self.busy = Some("Deleting…".to_string());
                    }
                    Some(RowAction::Edit(i)) if !entry.is_dir => {
                        self.start_edit(&cwd, &entry.name, i);
                    }
                    _ => {}
                }
            }
            ui.add_space(14.0);
        });

        // --- Status strip ------------------------------------------------------
        let (icon, color, text) = if self.busy.is_some() {
            ("○", theme::TEXT_DIM(), self.busy.clone().unwrap_or_default())
        } else {
            match &self.last_status {
                Some((ok, m)) => (
                    if *ok { "✓" } else { "!" },
                    if *ok { theme::SUCCESS() } else { theme::DANGER() },
                    m.clone(),
                ),
                None => ("", theme::TEXT_FAINT(), String::new()),
            }
        };
        if !text.is_empty() {
            ui.label(
                RichText::new(format!("{icon} {text}"))
                    .size(10.5)
                    .color(color),
            );
        } else {
            ui.label(
                RichText::new(
                    "Double-click a folder to open it, a file to download; right-click for more.",
                )
                .size(10.0)
                .color(theme::TEXT_FAINT()),
            );
        }

        // --- Modals ------------------------------------------------------------
        if self.folder_modal {
            let mut clicked: Option<String> = None;
            egui::Modal::new(egui::Id::new("fb_newfolder")).show(&self.ctx, |m| {
                m.label(RichText::new("New folder").size(14.0).strong());
                let resp = m.add(
                    egui::TextEdit::singleline(&mut self.folder_name)
                        .hint_text("backups")
                        .desired_width(260.0)
                        .margin(egui::vec2(8.0, 6.0)),
                );
                let submitted =
                    resp.lost_focus() && m.input(|i| i.key_pressed(egui::Key::Enter));
                m.add_space(8.0);
                m.horizontal(|ui| {
                    if ui.button("Create").clicked() || submitted {
                        clicked = Some(self.folder_name.trim().to_string());
                    }
                    if ui.button("Cancel").clicked() {
                        clicked = Some(String::new());
                    }
                });
            });
            if let Some(name) = clicked {
                self.folder_modal = false;
                if !name.is_empty() {
                    self.send(FbCmd::MkDir(path_join(&self.cwd, &name)));
                    self.busy = Some("Creating folder…".to_string());
                }
            }
        }
        if let Some((_cwd, old_name)) = self.rename_modal.clone() {
            let mut clicked: Option<String> = None;
            egui::Modal::new(egui::Id::new("fb_rename")).show(&self.ctx, |m| {
                m.label(RichText::new("Rename").size(14.0).strong());
                let resp = m.add(
                    egui::TextEdit::singleline(&mut self.rename_value)
                        .desired_width(260.0)
                        .margin(egui::vec2(8.0, 6.0)),
                );
                let submitted =
                    resp.lost_focus() && m.input(|i| i.key_pressed(egui::Key::Enter));
                m.add_space(8.0);
                m.horizontal(|ui| {
                    if ui.button("Rename").clicked() || submitted {
                        clicked = Some(self.rename_value.trim().to_string());
                    }
                    if ui.button("Cancel").clicked() {
                        clicked = Some(String::new());
                    }
                });
            });
            if let Some(new_name) = clicked {
                let old = old_name;
                self.rename_modal = None;
                if !new_name.is_empty() && new_name != old {
                    self.send(FbCmd::Rename {
                        from: path_join(&self.cwd, &old),
                        to: path_join(&self.cwd, &new_name),
                    });
                    self.busy = Some("Renaming…".to_string());
                }
            }
        }

        // --- Local editors modal -----------------------------------------------
        if self.editors_modal {
            let mut editors = self
                .editors
                .read()
                .map(|v| v.clone())
                .unwrap_or_default();
            let mut closed = false;
            let mut removed: Option<usize> = None;
            let mut added: Option<(String, String)> = None;
            let mut picked: Option<std::path::PathBuf> = None;
            egui::Modal::new(egui::Id::new("fb_editors")).show(&self.ctx, |m| {
                m.label(RichText::new("Local editors").size(14.0).strong());
                m.label(
                    RichText::new(
                        "Used by the right-click “Open with” menu. A remote file is\n\
                         downloaded to a temp copy, opened in the editor, and\n\
                         re-uploaded automatically when the editor exits.",
                    )
                    .size(10.0)
                    .color(theme::TEXT_FAINT()),
                );
                m.add_space(6.0);
                if editors.is_empty() {
                    m.label(
                        RichText::new("No editors configured yet — add one below.")
                            .size(11.0)
                            .color(theme::TEXT_DIM()),
                    );
                }
                for (i, e) in editors.iter().enumerate() {
                    m.horizontal(|ui| {
                        ui.label(RichText::new(&e.name).size(12.0));
                        ui.add(
                            egui::Label::new(
                                RichText::new(&e.path).size(9.0).color(theme::TEXT_FAINT()),
                            )
                            .truncate(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Remove").clicked() {
                                removed = Some(i);
                            }
                        });
                    });
                }
                m.add_space(6.0);
                m.separator();
                m.label(RichText::new("Add editor").size(11.0).color(theme::TEXT_FAINT()));
                m.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.ed_name)
                            .desired_width(90.0)
                            .hint_text("name"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.ed_path)
                            .desired_width(230.0)
                            .hint_text(r"C:\path\to\editor.exe"),
                    );
                    if ui
                        .small_button("…")
                        .on_hover_text("Browse for the executable")
                        .clicked()
                    {
                        picked = rfd::FileDialog::new().pick_file();
                    }
                });
                if ui
                    .button("Add")
                    .clicked()
                    && !self.ed_name.trim().is_empty()
                    && !self.ed_path.trim().is_empty()
                {
                    added = Some((self.ed_name.trim().to_string(), self.ed_path.trim().to_string()));
                }
                m.add_space(8.0);
                if ui.button("Done").clicked() {
                    closed = true;
                }
            });
            if let Some(p) = picked {
                self.ed_path = p.display().to_string();
            }
            if let Some(i) = removed {
                editors.remove(i);
            }
            if let Some((name, path)) = added {
                editors.push(EditorCfg { name, path });
                self.ed_name.clear();
                self.ed_path.clear();
            }
            if closed {
                self.editors_modal = false;
                if let Ok(mut w) = self.editors.write() {
                    *w = editors.clone();
                }
                save_editors(&editors);
            }
        }
    }

    fn start_download(&mut self, cwd: &str, name: &str, note: &str) {
        if let Some(local) = rfd::FileDialog::new().set_file_name(name).save_file() {
            self.send(FbCmd::Download {
                remote: path_join(cwd, name),
                local: local.display().to_string(),
            });
            self.busy = Some(note.to_string());
        }
    }

    /// WinSCP-style local edit: download to a temp copy, open it in the
    /// selected local editor, and re-upload automatically when that
    /// editor process exits.
    fn start_edit(&mut self, cwd: &str, file_name: &str, editor_idx: usize) {
        let editors = self
            .editors
            .read()
            .map(|v| v.clone())
            .unwrap_or_default();
        let Some(e) = editors.get(editor_idx) else {
            return;
        };
        let (ed_name, ed_path) = (e.name.clone(), e.path.clone());
        let remote = path_join(cwd, file_name);
        let mut local = std::env::temp_dir();
        local.push(format!(
            "culvert-edit-{}-{}",
            std::process::id(),
            base_name(&remote)
        ));
        self.send(FbCmd::Download {
            remote: remote.clone(),
            local: local.display().to_string(),
        });
        self.pending_edit = Some((remote, local, ed_name, ed_path));
        self.busy = Some("Downloading for edit…".to_string());
    }

    fn launch_editor(
        &mut self,
        remote: String,
        local: std::path::PathBuf,
        ed_name: String,
        ed_path: String,
    ) {
        let mut cmd = if ed_path.to_ascii_lowercase().ends_with(".cmd")
            || ed_path.to_ascii_lowercase().ends_with(".bat")
        {
            // .cmd/.bat wrappers (VS Code installer alias, …) need cmd.exe.
            let mut c = std::process::Command::new("cmd");
            c.arg("/C").arg(&ed_path);
            c
        } else {
            std::process::Command::new(&ed_path)
        };
        cmd.arg(&local);
        match cmd.spawn() {
            Ok(child) => {
                let file = base_name(&remote);
                self.edit_job = Some(EditJob {
                    file: file.clone(),
                    editor: ed_name.clone(),
                    remote,
                    local,
                    child,
                });
                push_log(
                    &self.logs,
                    LogEntry::new(
                        LogLevel::Info,
                        &self.title,
                        &format!("Opened {file} in {ed_name}"),
                    ),
                );
            }
            Err(e) => {
                self.last_status = Some((false, format!("Cannot start {ed_name}: {e}")));
            }
        }
    }

    /// Connect / error overlay, mirroring the terminal tab style.
    fn lifecycle_overlay(&mut self, ui: &mut Ui) {
        let rect = ui.available_rect_before_wrap();
        if let FbStatus::Connecting = &self.status {
            let fake = egui::Rect::from_center_size(rect.center(), egui::vec2(1.0, 1.0));
            ui.put(fake, egui::Spinner::new().size(22.0));
            ui.painter().text(
                rect.center() + egui::vec2(0.0, 30.0),
                Align2::CENTER_CENTER,
                format!("Opening SFTP on {} …", self.session.connection_label()),
                FontId::proportional(11.5),
                theme::TEXT_DIM(),
            );
        } else if let FbStatus::Failed(reason) = self.status.clone() {
            ui.painter()
                .rect_filled(rect.shrink(1.0), 0.0, Color32::from_black_alpha(120));
            let card = egui::Rect::from_center_size(rect.center(), egui::vec2(430.0, 150.0));
            ui.painter().rect_filled(card, 12.0, theme::BG_CARD());
            ui.painter().rect_stroke(
                card,
                12.0,
                Stroke::new(1.0_f32, theme::BORDER_STRONG()),
                egui::StrokeKind::Inside,
            );
            ui.painter().text(
                card.center() + egui::vec2(0.0, -48.0),
                Align2::CENTER_CENTER,
                "File browser failed",
                FontId::proportional(14.0),
                theme::TEXT(),
            );
            let reason_rect = egui::Rect::from_min_size(
                card.min + egui::vec2(28.0, card.height() * 0.34),
                egui::vec2(card.width() - 56.0, 48.0),
            );
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(reason_rect));
            child.vertical_centered(|ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(reason.clone()).size(11.5).color(theme::TEXT_DIM()),
                    )
                    .wrap(),
                );
            });
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(
                egui::Rect::from_center_size(
                    card.center() + egui::vec2(0.0, 44.0),
                    egui::vec2(280.0, 30.0),
                ),
            ));
            child.horizontal_centered(|ui| {
                if ui.button("Retry").clicked() {
                    self.reconnect();
                }
                if ui.button("Close tab").clicked() {
                    self.want_close = true;
                }
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Row
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum RowAction {
    Select,
    Open,
    Download,
    Rename,
    Delete,
    Edit(usize),
}

/// One file/dir line: icon + name (elided); size and mtime appended at fixed
/// columns when the container is wide enough (they'd clip in the sidebar).
fn file_row(
    ui: &mut Ui,
    entry: &FbEntry,
    selected: bool,
    editors: &[EditorCfg],
) -> Option<RowAction> {
    let mut action: Option<RowAction> = None;
    let row_w = ui.available_width();
    let show_cols = row_w >= 280.0;
    let fr = Frame::NONE
        .fill(if selected {
            theme::BG_CARD_SEL()
        } else {
            Color32::TRANSPARENT
        })
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (tile, _) = ui.allocate_exact_size(egui::vec2(14.0, 16.0), Sense::hover());
                ui.painter().text(
                    tile.center(),
                    Align2::CENTER_CENTER,
                    if entry.is_dir { "▸" } else { "·" },
                    FontId::proportional(11.0),
                    if entry.is_dir { theme::INFO() } else { theme::TEXT_FAINT() },
                );
                ui.add(egui::Label::new(RichText::new(&entry.name).size(12.0)).truncate());
            });
        });
    let rr = fr.response.rect;
    let size_x = rr.left() + rr.width() * 0.58;
    let y = rr.center().y;
    let p = ui.painter();
    if !show_cols {
        return action;
    }
    if !entry.is_dir {
        p.text(
            egui::pos2(size_x, y),
            Align2::LEFT_CENTER,
            human_size(entry.size),
            FontId::proportional(10.5),
            theme::TEXT_DIM(),
        );
    }
    p.text(
        egui::pos2(size_x + 90.0, y),
        Align2::LEFT_CENTER,
        mtime_label(entry.mtime),
        FontId::proportional(10.5),
        theme::TEXT_FAINT(),
    );

    let resp = fr.response.interact(Sense::click());
    if resp.clicked() {
        action = Some(RowAction::Select);
    }
    if resp.double_clicked() {
        action = Some(RowAction::Open);
    }
    resp.context_menu(|ui| {
        if entry.is_dir && ui.button("Open").clicked() {
            action = Some(RowAction::Open);
            ui.close();
        }
        if !entry.is_dir {
            for (i, e) in editors.iter().enumerate() {
                let label = if e.name.is_empty() {
                    format!("Open with editor {}", i + 1)
                } else {
                    format!("Open with {}", e.name)
                };
                if ui.button(label).clicked() {
                    action = Some(RowAction::Edit(i));
                    ui.close();
                }
            }
            if ui.button("Download…").clicked() {
                action = Some(RowAction::Download);
                ui.close();
            }
        }
        if ui.button("Rename…").clicked() {
            action = Some(RowAction::Rename);
            ui.close();
        }
        if ui.button("Delete").clicked() {
            action = Some(RowAction::Delete);
            ui.close();
        }
    });
    action
}

// ---------------------------------------------------------------------------
// Async SFTP task
// ---------------------------------------------------------------------------

async fn sftp_task(
    session: SshSession,
    logs: SharedLogs,
    ctx: egui::Context,
    evt_tx: UnboundedSender<FbEvent>,
    mut cmd_rx: UnboundedReceiver<FbCmd>,
) {
    let failed = |evt_tx: &UnboundedSender<FbEvent>, ctx: &egui::Context, msg: String| {
        let _ = evt_tx.send(FbEvent::Failed(msg));
        ctx.request_repaint();
    };

    let (ftx, _frx) = tokio::sync::mpsc::unbounded_channel();
    let handle = match ssh::connect(&session, logs.clone(), ftx).await {
        Ok(h) => h,
        Err(e) => return failed(&evt_tx, &ctx, format!("{e:#}")),
    };
    let ch = match handle.channel_open_session().await {
        Ok(c) => c,
        Err(e) => return failed(&evt_tx, &ctx, format!("Cannot open channel: {e}")),
    };
    if let Err(e) = ch.request_subsystem(true, "sftp").await {
        return failed(&evt_tx, &ctx, format!("Server has no SFTP subsystem: {e}"));
    }
    let sftp = match SftpSession::new(ch.into_stream()).await {
        Ok(s) => s,
        Err(e) => return failed(&evt_tx, &ctx, format!("SFTP init failed: {e}")),
    };

    push_log(
        &logs,
        LogEntry::new(LogLevel::Success, &session.name, "SFTP session opened"),
    );

    // The server-side current directory; every command below keeps it fresh.
    let mut cur = match sftp.canonicalize(".").await {
        Ok(c) => c,
        Err(e) => return failed(&evt_tx, &ctx, format!("Cannot resolve home dir: {e}")),
    };
    match list_at(&sftp, &cur).await {
        Ok((cwd, entries)) => {
            cur = cwd;
            let _ = evt_tx.send(FbEvent::Connected { cwd: cur.clone(), entries });
            ctx.request_repaint();
        }
        Err(e) => return failed(&evt_tx, &ctx, format!("Listing failed: {e:#}")),
    }

    let op_failed = |evt_tx: &UnboundedSender<FbEvent>, ctx: &egui::Context, msg: String| {
        let _ = evt_tx.send(FbEvent::OpFailed(msg));
        ctx.request_repaint();
    };

    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            FbCmd::Navigate(path) => match list_at(&sftp, &path).await {
                Ok((cwd, entries)) => {
                    cur = cwd;
                    let _ = evt_tx.send(FbEvent::Listing { cwd: cur.clone(), entries });
                    ctx.request_repaint();
                }
                Err(e) => op_failed(&evt_tx, &ctx, format!("Cannot open {path}: {e:#}")),
            },
            FbCmd::Up => {
                let parent = path_parent(&cur);
                match list_at(&sftp, &parent).await {
                    Ok((cwd, entries)) => {
                        cur = cwd;
                        let _ = evt_tx.send(FbEvent::Listing { cwd: cur.clone(), entries });
                        ctx.request_repaint();
                    }
                    Err(e) => op_failed(&evt_tx, &ctx, format!("Cannot go up: {e:#}")),
                }
            }
            FbCmd::Refresh => match list_at(&sftp, &cur).await {
                Ok((cwd, entries)) => {
                    cur = cwd;
                    let _ = evt_tx.send(FbEvent::Listing { cwd: cur.clone(), entries });
                    ctx.request_repaint();
                }
                Err(e) => op_failed(&evt_tx, &ctx, format!("Refresh failed: {e:#}")),
            },
            FbCmd::MkDir(path) => match sftp.create_dir(&path).await {
                Ok(_) => {
                    relist(&sftp, &cur, &evt_tx, &ctx, "Folder created").await;
                }
                Err(e) => op_failed(&evt_tx, &ctx, format!("mkdir failed: {e}")),
            },
            FbCmd::Remove { path, is_dir } => {
                let res = if is_dir {
                    sftp.remove_dir(&path).await
                } else {
                    sftp.remove_file(&path).await
                };
                match res {
                    Ok(_) => {
                        relist(&sftp, &cur, &evt_tx, &ctx, "Deleted").await;
                    }
                    Err(e) => op_failed(&evt_tx, &ctx, format!("Delete failed: {e}")),
                }
            }
            FbCmd::Rename { from, to } => match sftp.rename(&from, &to).await {
                Ok(_) => {
                    relist(&sftp, &cur, &evt_tx, &ctx, "Renamed").await;
                }
                Err(e) => op_failed(&evt_tx, &ctx, format!("Rename failed: {e}")),
            },
            FbCmd::Download { remote, local } => {
                let t0 = std::time::Instant::now();
                let res = async {
                    let mut rf = sftp.open(&remote).await?;
                    let mut lf = tokio::fs::File::create(&local).await?;
                    tokio::io::copy(&mut rf, &mut lf)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                match res {
                    Ok(()) => {
                        let _ = evt_tx.send(FbEvent::Done(format!(
                            "Downloaded {} ({}) in {:.1}s",
                            base_name(&remote),
                            local,
                            t0.elapsed().as_secs_f32()
                        )));
                        ctx.request_repaint();
                    }
                    Err(e) => op_failed(&evt_tx, &ctx, format!("Download failed: {e:#}")),
                }
            }
            FbCmd::Upload { remote, local } => {
                let t0 = std::time::Instant::now();
                let res = async {
                    let mut lf = tokio::fs::File::open(&local).await?;
                    let mut rf = sftp
                        .open_with_flags(
                            &remote,
                            OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
                        )
                        .await?;
                    tokio::io::copy(&mut lf, &mut rf)
                        .await
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                match res {
                    Ok(()) => {
                        relist(&sftp, &cur, &evt_tx, &ctx, "Upload complete").await;
                        let _ = evt_tx.send(FbEvent::Done(format!(
                            "Uploaded {} in {:.1}s",
                            base_name(&local),
                            t0.elapsed().as_secs_f32()
                        )));
                        ctx.request_repaint();
                    }
                    Err(e) => op_failed(&evt_tx, &ctx, format!("Upload failed: {e:#}")),
                }
            }
            FbCmd::Close => break,
        }
    }
}

/// Re-list the current directory after a mutating operation so the table
/// stays in sync, then surface the completion note.
async fn relist(
    sftp: &SftpSession,
    cur: &str,
    evt_tx: &UnboundedSender<FbEvent>,
    ctx: &egui::Context,
    msg: &str,
) {
    if let Ok((cwd, entries)) = list_at(sftp, cur).await {
        let _ = evt_tx.send(FbEvent::Listing { cwd, entries });
    }
    let _ = evt_tx.send(FbEvent::Done(msg.to_string()));
    ctx.request_repaint();
}

async fn list_at(sftp: &SftpSession, path: &str) -> anyhow::Result<(String, Vec<FbEntry>)> {
    let canon = sftp
        .canonicalize(path)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let dir = if canon.is_empty() { "/".to_string() } else { canon.clone() };
    let mut entries = Vec::new();
    for entry in sftp.read_dir(&dir).await? {
        let md = entry.metadata();
        entries.push(FbEntry {
            name: entry.file_name().to_string(),
            is_dir: md.is_dir(),
            size: md.size.unwrap_or(0),
            mtime: md.mtime.unwrap_or(0),
        });
    }
    // Folders first, then names (case-insensitive, like WinSCP).
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok((dir, entries))
}

fn base_name(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

// ---------------------------------------------------------------------------
// Path helpers (SFTP paths are server-side, always '/'-separated)
// ---------------------------------------------------------------------------

pub fn path_join(base: &str, name: &str) -> String {
    let base = base.trim();
    let name = name.trim().trim_start_matches('/');
    if name.is_empty() {
        return base.to_string();
    }
    if base.is_empty() || base == "/" {
        format!("/{name}")
    } else if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

pub fn path_parent(p: &str) -> String {
    let p = p.trim();
    let p = p.trim_end_matches('/');
    let p = if p.is_empty() { "/" } else { p };
    match p.rfind('/') {
        None => "/".to_string(),
        Some(0) => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

/// Byte size for the SIZE column.
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * KB;
    const GB: f64 = MB * KB;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

fn mtime_label(secs: u32) -> String {
    if secs == 0 {
        return String::new();
    }
    chrono::DateTime::from_timestamp(secs as i64, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_join_builds_server_paths() {
        assert_eq!(path_join("/", "etc"), "/etc");
        assert_eq!(path_join("/etc", "nginx"), "/etc/nginx");
        assert_eq!(path_join("/etc/", "nginx"), "/etc/nginx");
        assert_eq!(path_join("/etc", ""), "/etc");
        assert_eq!(path_join("/", ""), "/");
        assert_eq!(path_join("/", "/etc"), "/etc");
    }

    #[test]
    fn path_parent_walks_up() {
        assert_eq!(path_parent("/etc/nginx"), "/etc");
        assert_eq!(path_parent("/etc"), "/");
        assert_eq!(path_parent("/"), "/");
        assert_eq!(path_parent("/etc/"), "/");
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024 * 3), "3.0 KB");
        assert_eq!(human_size(1024 * 1024 * 5), "5.0 MB");
    }

    // Temporary live probe against the docker test server. Ignored by
    // default: run with `cargo test -- --ignored --nocapture sftp_probe`.
    #[test]
    #[ignore = "needs the local docker ssh test server on 127.0.0.1:2222"]
    fn sftp_probe() {
        use crate::session::{AuthType, SshSession};
        use crate::ssh;

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let session = SshSession {
                name: "probe".into(),
                host: "127.0.0.1".into(),
                port: 2222,
                username: "linuxio".into(),
                auth_type: AuthType::Password,
                password: "test123".into(),
                ..SshSession::default()
            };
            let logs: SharedLogs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let (ftx, _frx) = tokio::sync::mpsc::unbounded_channel();
            println!("stage 1: ssh::connect");
            let handle = ssh::connect(&session, logs, ftx)
                .await
                .expect("connect failed");
            println!("stage 2: channel_open_session");
            let ch = handle.channel_open_session().await.expect("no channel");
            println!("stage 3: request_subsystem sftp");
            ch.request_subsystem(true, "sftp").await.expect("no sftp");
            println!("stage 4: SftpSession::new");
            let sftp = SftpSession::new(ch.into_stream()).await.expect("sftp init");
            println!("stage 5: canonicalize");
            let cwd = sftp.canonicalize(".").await.expect("canonicalize");
            println!("home = {cwd:?}");
            println!("stage 6: read_dir");
            let mut entries = sftp.read_dir(&cwd).await.expect("read_dir");
            for _ in 0..10 {
                let Some(e) = entries.next() else { break };
                println!("- {:?} {:?}", e.file_name(), e.file_type());
            }
            println!("ALL_STAGES_OK");
        });
    }
}
