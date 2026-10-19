//! WinSCP-style remote file browser: opens its own SFTP subsystem using the
//! saved SSH credentials and renders a navigable file table with upload,
//! download, rename, delete and folder creation. Original UI (egui), no
//! external file-manager code or assets.

use crate::log::{push_log, LogEntry, LogLevel, SharedLogs};
use crate::session::SshSession;
use crate::ssh;
use crate::theme;
use crate::theme::tglyph;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Frame, Margin, RichText, Sense, Stroke, Ui,
};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
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
    /// Create an empty remote file (dock "New file" action).
    MkFile(String),
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

/// An in-flight "download, open in editor, sync back" job. The local copy is
/// watched while the editor runs: every save re-uploads immediately, and the
/// editor exiting pushes the final state (WinSCP-style). Editors that detach
/// via a short-lived launcher (.cmd/.bat wrappers like the VS Code alias) are
/// detected and simply keep being watched without the process handle.
struct EditJob {
    /// Base file name, for the status note.
    file: String,
    editor: String,
    /// Remote absolute path to re-upload to.
    remote: String,
    local: std::path::PathBuf,
    child: Option<std::process::Child>,
    /// Local copy fingerprint when the job started: a save shows up as a
    /// differing (len, mtime). Re-snapshotted after each upload.
    snap_len: u64,
    snap_mtime: Option<std::time::SystemTime>,
    /// Throttle for the metadata checks (poll every ~700 ms).
    next_check: std::time::Instant,
    /// When the job started, to tell real editors from quick wrappers apart.
    started: std::time::Instant,
    /// Last detected save (wrapper mode: stop watching after 30 idle minutes).
    last_change_seen: std::time::Instant,
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
    /// Selected listing entries (Ctrl/Shift+click multi-select); the anchor
    /// is where shift-ranges start from.
    selected: BTreeSet<String>,
    anchor: Option<String>,
    /// Reveal dot-files that the listing normally hides.
    show_hidden: bool,
    /// Operation in flight; shows a busy note and gates repeat actions.
    busy: Option<String>,
    last_status: Option<(bool, String)>,
    /// "New folder" modal open + typed name.
    folder_modal: bool,
    folder_name: String,
    /// "New file" modal open + typed name.
    file_modal: bool,
    file_name: String,
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
            selected: BTreeSet::new(),
            anchor: None,
            show_hidden: false,
            busy: None,
            last_status: None,
            folder_modal: false,
            folder_name: String::new(),
            file_modal: false,
            file_name: String::new(),
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
        self.selected.clear();
        self.anchor = None;
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
                    self.selected.clear();
                    self.anchor = None;
                    changed = true;
                }
                FbEvent::Listing { cwd, entries } => {
                    self.cwd = cwd.clone();
                    self.path_edit = cwd;
                    self.entries = entries;
                    self.selected.clear();
                    self.anchor = None;
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
        // Watching an open edit job: every save re-uploads right away, and
        // the editor's exit pushes the final state (WinSCP-style). Actions
        // that touch `self` are collected into flags and run after the job
        // borrow ends.
        let mut save_upload: Option<(String, String)> = None;
        let mut exit_upload: Option<(String, String, String, String)> = None;
        let mut stop_watching: Option<String> = None;
        let mut end_edit_note: Option<(String, String)> = None;
        let mut job_alive = false;
        if let Some(job) = &mut self.edit_job {
            let now = std::time::Instant::now();
            if now >= job.next_check {
                if let Ok(md) = std::fs::metadata(&job.local) {
                    let mtime = md.modified().ok();
                    if md.len() != job.snap_len || mtime != job.snap_mtime {
                        // The user saved: push a fresh copy to the server.
                        save_upload = Some((
                            job.remote.clone(),
                            job.local.display().to_string(),
                        ));
                        job.snap_len = md.len();
                        job.snap_mtime = mtime;
                        // Give the SFTP write a moment before the next probe.
                        job.next_check = now + std::time::Duration::from_secs(2);
                        job.last_change_seen = now;
                        changed = true;
                    } else {
                        job.next_check = now + std::time::Duration::from_millis(700);
                    }
                } else {
                    job.next_check = now + std::time::Duration::from_millis(700);
                }
            }
            if let Some(child) = job.child.as_mut() {
                if let Ok(Some(_)) = child.try_wait() {
                    if job.started.elapsed() < std::time::Duration::from_secs(5) {
                        // A .cmd/.bat launcher that spawned a detached editor
                        // (VS Code alias, …): the process exited immediately,
                        // so keep watching the file for saves and stop only
                        // after a long idle window.
                        job.child = None;
                        job_alive = true;
                    } else {
                        exit_upload = Some((
                            job.file.clone(),
                            job.editor.clone(),
                            job.remote.clone(),
                            job.local.display().to_string(),
                        ));
                    }
                } else {
                    job_alive = true;
                }
            }
            if job.child.is_none() && stop_watching.is_none() {
                // Wrapper-launched editor: stop watching after a long idle
                // window so dead watchers don't linger forever.
                if job.last_change_seen.elapsed() > std::time::Duration::from_secs(30 * 60) {
                    stop_watching = Some(job.file.clone());
                } else {
                    job_alive = true;
                }
            }
        }
        if exit_upload.is_some() || stop_watching.is_some() {
            self.edit_job = None;
        }
        if let Some((file, editor, remote, local)) = exit_upload {
            self.edit_job = None;
            self.send(FbCmd::Upload { remote, local });
            end_edit_note = Some((file, editor));
            changed = true;
        }
        if let Some((remote, local)) = save_upload {
            self.send(FbCmd::Upload { remote, local });
            changed = true;
        }
        if let Some(file) = stop_watching {
            self.edit_job = None;
            self.last_status = Some((true, format!("Stopped watching {file}")));
            changed = true;
        }
        if end_edit_note.is_some() {
            let (file, editor) = end_edit_note.clone().unwrap();
            self.busy =
                Some(format!("Editing of {file} closed ({editor}) — re-uploading…"));
        }
        if job_alive {
            // Keep ticking while the editor is open even when the app is
            // otherwise idle (no repaints scheduled by the input).
            self.ctx.request_repaint_after(std::time::Duration::from_millis(500));
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
        // One row of proper icon tools: parent, refresh, upload, download,
        // copy path, delete, show-hidden toggle, new file, new folder and the
        // editors manager. Tiles shrink to the available dock width so the
        // whole set stays on the single row (no wrapping, no scroll bar).
        let tools_avail = ui.available_width();
        // needed width = 10 tiles + 9 one-px gaps
        let tile_w = ((tools_avail - 9.0) / 10.0).floor().clamp(13.0, 17.0);
        Frame::NONE
            .fill(theme::BG_CARD())
            .corner_radius(CornerRadius::same(10))
            .stroke(Stroke::new(1.0_f32, theme::BORDER()))
            .inner_margin(Margin::symmetric(6, 4))
            .show(ui, |ui| {
                // The row always fits by construction; keep the BeltSafe
                // hidden so a stray rounding pixel can't summon the fat
                // floating thumb over the address bar.
                egui::ScrollArea::horizontal()
                    .id_salt("fb_tool_strip")
                    .auto_shrink([false, true])
                    .scroll_bar_visibility(
                        egui::scroll_area::ScrollBarVisibility::AlwaysHidden,
                    )
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 1.0;

                            if dock_button(
                                ui,
                                tglyph::CHEVRON_UP,
                                theme::TEXT_DIM(),
                                "Parent folder",
                                tile_w,
                                true,
                            )
                            .clicked()
                            {
                                self.send(FbCmd::Up);
                            }
                            if dock_button(
                                ui,
                                tglyph::REFRESH,
                                theme::TEXT_DIM(),
                                "Refresh",
                                tile_w,
                                true,
                            )
                            .clicked()
                            {
                                self.send(FbCmd::Refresh);
                            }
                            let busy = self.busy.is_some();
                            if dock_button(
                                ui,
                                tglyph::UPLOAD,
                                theme::INFO(),
                                "Upload files… (multi-select)",
                                tile_w,
                                !busy,
                            )
                            .clicked()
                            {
                                self.upload_many();
                            }
                            let has_files = !self.selected_files().is_empty();
                            if dock_button(
                                ui,
                                tglyph::DOWNLOAD,
                                theme::INFO(),
                                "Download selected files… (multi-select)",
                                tile_w,
                                !busy && has_files,
                            )
                            .clicked()
                            {
                                self.download_many();
                            }
                            let has_sel = !self.selected.is_empty();
                            if dock_button(
                                ui,
                                tglyph::COPY,
                                theme::TEXT_DIM(),
                                "Copy path of selection",
                                tile_w,
                                has_sel,
                            )
                            .clicked()
                            {
                                let paths: Vec<String> = self
                                    .selected
                                    .iter()
                                    .map(|n| path_join(&self.cwd, n))
                                    .collect();
                                ui.ctx().copy_text(paths.join("\r\n"));
                                self.ctx.request_repaint();
                                let n = paths.len();
                                self.last_status = Some((true, {
                                    if n == 1 {
                                        "Path copied to clipboard".to_string()
                                    } else {
                                        format!("{n} paths copied to clipboard")
                                    }
                                }));
                            }
                            if dock_button(
                                ui,
                                tglyph::TRASH,
                                theme::DANGER(),
                                "Delete selection",
                                tile_w,
                                has_sel,
                            )
                            .clicked()
                            {
                                self.delete_selected();
                            }
                            if dock_button(
                                ui,
                                if self.show_hidden {
                                    tglyph::EYE
                                } else {
                                    tglyph::EYE_OFF
                                },
                                if self.show_hidden {
                                    theme::ACCENT()
                                } else {
                                    theme::TEXT_DIM()
                                },
                                if self.show_hidden {
                                    "Hide dot-files"
                                } else {
                                    "Show hidden (dot) files"
                                },
                                tile_w,
                                true,
                            )
                            .clicked()
                            {
                                self.show_hidden = !self.show_hidden;
                                self.selected.clear();
                                self.anchor = None;
                                ui.ctx().request_repaint();
                            }
                            if dock_button(
                                ui,
                                tglyph::FILE_PLUS,
                                theme::TEXT_DIM(),
                                "Create new file",
                                tile_w,
                                true,
                            )
                            .clicked()
                            {
                                self.file_modal = true;
                                self.file_name.clear();
                            }
                            if dock_button(
                                ui,
                                tglyph::FOLDER_PLUS,
                                theme::TEXT_DIM(),
                                "Create new folder",
                                tile_w,
                                true,
                            )
                            .clicked()
                            {
                                self.folder_modal = true;
                                self.folder_name.clear();
                            }
                            if dock_button(
                                ui,
                                tglyph::PENCIL,
                                theme::TEXT_DIM(),
                                "Local editors…",
                                tile_w,
                                true,
                            )
                            .clicked()
                            {
                                self.editors_modal = true;
                            }
                        });
                    });

                ui.add_space(3.0);
                // Address bar under the dock: shows the current path; edit it
                // and press Enter to switch directories.
                let path_resp = ui.add(
                    egui::TextEdit::singleline(&mut self.path_edit)
                        .desired_width(ui.available_width())
                        .font(FontId::monospace(11.0))
                        .margin(egui::vec2(6.0, 4.0)),
                );
                if path_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    let p = self.path_edit.trim().to_string();
                    if !p.is_empty() {
                        self.send(FbCmd::Navigate(p));
                    }
                } else if path_resp.gained_focus() && self.path_edit.trim().is_empty() {
                    self.path_edit = self.cwd.clone();
                }
            });
        ui.add_space(6.0);

        // --- Status strip ------------------------------------------------------
        // Kept above the list so busy/error bubbles are never clipped by the
        // sidebar's bottom edge when the listing is tall.
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
                    "Double-click opens a folder or the file in your editor (temp\
                     copy, re-uploaded when the editor closes). Ctrl+click selects\
                     extra items, Shift+click a range; right-click for more.",
                )
                .size(10.0)
                .color(theme::TEXT_FAINT()),
            );
        }

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
            // Dot-files stay hidden unless the eye tool is on; "." and ".."
            // are never shown.
            let visible: Vec<FbEntry> = self
                .entries
                .iter()
                .filter(|e| {
                    (self.show_hidden || !e.name.starts_with('.'))
                        && e.name != "."
                        && e.name != ".."
                })
                .cloned()
                .collect();
            let cwd = self.cwd.clone();
            let editors = self
                .editors
                .read()
                .map(|v| v.clone())
                .unwrap_or_default();
            for entry in &visible {
                let sel = self.selected.contains(&entry.name);
                let action = file_row(ui, entry, sel, &editors);
                match action {
                    Some(RowAction::Select(kind)) => {
                        let name = entry.name.clone();
                        match kind {
                            SelectMod::Plain => {
                                self.selected.clear();
                                self.selected.insert(name.clone());
                                self.anchor = Some(name);
                            }
                            SelectMod::Toggle => {
                                if !self.selected.remove(&name) {
                                    self.selected.insert(name.clone());
                                }
                                self.anchor = Some(name);
                            }
                            SelectMod::Range => {
                                let a = self.anchor.clone();
                                let end = visible.iter().position(|e| e.name == name);
                                let start = a
                                    .as_ref()
                                    .and_then(|a| visible.iter().position(|e| &e.name == a));
                                if let (Some(s), Some(t)) = (start, end) {
                                    self.selected.clear();
                                    for e in &visible[s.min(t)..=s.max(t)] {
                                        self.selected.insert(e.name.clone());
                                    }
                                    // Keep the anchor so further shifts pivot around it.
                                } else {
                                    self.selected.insert(name.clone());
                                    self.anchor = Some(name);
                                }
                            }
                        }
                    }
                    Some(RowAction::Open) if entry.is_dir => {
                        self.send(FbCmd::Navigate(path_join(&cwd, &entry.name)));
                    }
                    Some(RowAction::Open) => {
                        // Double-click opens the file in the local editor
                        // (WinSCP-style: temp copy + re-upload on close).
                        // Plain downloads stay on the menu / download button.
                        let editors = self
                            .editors
                            .read()
                            .map(|v| v.clone())
                            .unwrap_or_default();
                        if let Some(e) = editors.first() {
                            self.start_edit_with(
                                &cwd,
                                &entry.name,
                                e.name.clone(),
                                e.path.clone(),
                            );
                        } else {
                            // No editor configured yet: Notepad keeps the
                            // double-click opens-an-editor contract.
                            self.start_edit_with(
                                &cwd,
                                &entry.name,
                                "Notepad".to_string(),
                                "notepad.exe".to_string(),
                            );
                        }
                    }
                    Some(RowAction::Download) => {
                        self.start_download(&cwd, &entry.name, "Downloading…");
                    }
                    Some(RowAction::Rename) => {
                        self.rename_modal = Some((cwd.clone(), entry.name.clone()));
                        self.rename_value = entry.name.clone();
                    }
                    Some(RowAction::Delete) => {
                        self.selected.remove(&entry.name);
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
                let esc = m.input(|i| i.key_pressed(egui::Key::Escape));
                m.horizontal(|ui| {
                    if ui.button("Create").clicked() || submitted {
                        clicked = Some(self.folder_name.trim().to_string());
                    }
                    if ui.button("Cancel").clicked() || esc {
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
        if self.file_modal {
            let mut clicked: Option<String> = None;
            egui::Modal::new(egui::Id::new("fb_newfile")).show(&self.ctx, |m| {
                m.label(RichText::new("New file").size(14.0).strong());
                let resp = m.add(
                    egui::TextEdit::singleline(&mut self.file_name)
                        .hint_text("notes.txt")
                        .desired_width(260.0)
                        .margin(egui::vec2(8.0, 6.0)),
                );
                let submitted =
                    resp.lost_focus() && m.input(|i| i.key_pressed(egui::Key::Enter));
                m.add_space(8.0);
                let esc = m.input(|i| i.key_pressed(egui::Key::Escape));
                m.horizontal(|ui| {
                    if ui.button("Create").clicked() || submitted {
                        clicked = Some(self.file_name.trim().to_string());
                    }
                    if ui.button("Cancel").clicked() || esc {
                        clicked = Some(String::new());
                    }
                });
            });
            if let Some(name) = clicked {
                self.file_modal = false;
                if !name.is_empty() {
                    self.send(FbCmd::MkFile(path_join(&self.cwd, &name)));
                    self.busy = Some("Creating file…".to_string());
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
                let esc = m.input(|i| i.key_pressed(egui::Key::Escape));
                m.horizontal(|ui| {
                    if ui.button("Rename").clicked() || submitted {
                        clicked = Some(self.rename_value.trim().to_string());
                    }
                    if ui.button("Cancel").clicked() || esc {
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
                let esc = m.input(|i| i.key_pressed(egui::Key::Escape));
                m.horizontal(|ui| {
                    if ui
                        .button("Add")
                        .clicked()
                        && !self.ed_name.trim().is_empty()
                        && !self.ed_path.trim().is_empty()
                    {
                        added = Some((self.ed_name.trim().to_string(), self.ed_path.trim().to_string()));
                    }
                    if ui.button("Done").clicked() {
                        closed = true;
                    }
                    if ui
                        .button("✕")
                        .on_hover_text("Close without saving")
                        .clicked()
                        || esc
                    {
                        closed = true;
                    }
                });
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

    /// Names of selected entries that are regular files (downloadable).
    fn selected_files(&self) -> Vec<String> {
        self.selected
            .iter()
            .filter(|n| {
                self.entries
                    .iter()
                    .any(|e| &e.name == *n && !e.is_dir)
            })
            .cloned()
            .collect()
    }

    /// Multi-file upload: pick a whole batch of local files at once, every
    /// one is queued into this directory.
    fn upload_many(&mut self) {
        let paths = rfd::FileDialog::new().pick_files();
        let Some(paths) = paths.filter(|p| !p.is_empty()) else {
            return;
        };
        let n = paths.len();
        for p in &paths {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "file.bin".to_string());
            self.send(FbCmd::Upload {
                remote: path_join(&self.cwd, &name),
                local: p.display().to_string(),
            });
        }
        self.busy = Some(if n == 1 {
            "Uploading…".to_string()
        } else {
            format!("Uploading {n} files…")
        });
    }

    /// Multi-file download: every selected regular file lands in one chosen
    /// local folder. Selected folders are skipped (SFTP has no FAST-dir copy).
    fn download_many(&mut self) {
        let files = self.selected_files();
        if files.is_empty() {
            return;
        }
        let Some(dir) = rfd::FileDialog::new()
            .set_title("Choose a folder for the downloads")
            .pick_folder()
        else {
            return;
        };
        let skipped = self.selected.len() - files.len();
        let n = files.len();
        for name in &files {
            self.send(FbCmd::Download {
                remote: path_join(&self.cwd, name),
                local: dir.join(name).display().to_string(),
            });
        }
        self.busy = Some(match (n, skipped) {
            (1, 0) => "Downloading…".to_string(),
            (1, _) => "Downloading 1 file (folders unsupported)…".to_string(),
            (n, 0) => format!("Downloading {n} files…"),
            (n, _) => format!("Downloading {n} files (folders unsupported)…"),
        });
    }

    /// Delete every selected entry (files and folders).
    fn delete_selected(&mut self) {
        let names: Vec<String> = self.selected.iter().cloned().collect();
        let mut queue: Vec<(String, bool)> = Vec::new();
        for n in &names {
            if let Some(e) = self.entries.iter().find(|e| &e.name == n).cloned() {
                queue.push((path_join(&self.cwd, &e.name), e.is_dir));
            }
        }
        if queue.is_empty() {
            return;
        }
        let n = queue.len();
        for (path, is_dir) in queue {
            self.send(FbCmd::Remove { path, is_dir });
        }
        self.busy = Some(if n == 1 {
            "Deleting…".to_string()
        } else {
            format!("Deleting {n} items…")
        });
        self.selected.clear();
        self.anchor = None;
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
        self.start_edit_with(cwd, file_name, e.name.clone(), e.path.clone());
    }

    /// Same as `start_edit` but with the editor resolved by the caller.
    fn start_edit_with(&mut self, cwd: &str, file_name: &str, ed_name: String, ed_path: String) {
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
        // Snapshot the downloaded copy so the save-watcher can tell an editor
        // save apart from the initial state.
        let (snap_len, snap_mtime) = match std::fs::metadata(&local) {
            Ok(m) => (m.len(), m.modified().ok()),
            Err(_) => (0, None),
        };
        match cmd.spawn() {
            Ok(child) => {
                let file = base_name(&remote);
                self.edit_job = Some(EditJob {
                    file: file.clone(),
                    editor: ed_name.clone(),
                    remote,
                    local,
                    child: Some(child),
                    snap_len,
                    snap_mtime,
                    next_check: std::time::Instant::now() + std::time::Duration::from_millis(500),
                    started: std::time::Instant::now(),
                    last_change_seen: std::time::Instant::now(),
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

/// How a row click should mutate the multi-selection.
#[derive(Clone, Copy)]
enum SelectMod {
    /// Plain click: select only this entry.
    Plain,
    /// Ctrl+click: add/remove this entry (also moves the range anchor).
    Toggle,
    /// Shift+click: extend the range from the anchor to this entry.
    Range,
}

#[derive(Clone, Copy)]
enum RowAction {
    Select(SelectMod),
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
                    if entry.is_dir {
                        tglyph::FOLDER_ROW
                    } else {
                        tglyph::FILE_ROW
                    },
                    theme::icon(11.0),
                    if entry.is_dir { theme::INFO() } else { theme::TEXT_FAINT() },
                );
                ui.add(egui::Label::new(RichText::new(&entry.name).size(12.0)).truncate());
            });
        });
    let rr = fr.response.rect;
    let size_x = rr.left() + rr.width() * 0.58;
    let y = rr.center().y;
    let p = ui.painter();
    if show_cols {
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
    }

    let resp = fr.response.interact(Sense::click());
    // Ctrl+click toggles, Shift+click extends the range (WinSCP behaviour).
    let mods = ui.input(|i| i.modifiers);
    if resp.clicked() {
        let kind = if mods.ctrl {
            SelectMod::Toggle
        } else if mods.shift {
            SelectMod::Range
        } else {
            SelectMod::Plain
        };
        action = Some(RowAction::Select(kind));
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
            FbCmd::MkFile(path) => match sftp
                .open_with_flags(&path, OpenFlags::CREATE | OpenFlags::WRITE)
                .await
            {
                Ok(file) => {
                    // Close (releases the handle) then re-list.
                    let _ = file.close().await;
                    relist(&sftp, &cur, &evt_tx, &ctx, "File created").await;
                }
                Err(e) => op_failed(&evt_tx, &ctx, format!("Create file failed: {e}")),
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
// Tool button
// ---------------------------------------------------------------------------

/// Flat dock tool: an adaptive square with a Tabler glyph; borderless until
/// hovered (soft accent wash), dimmed and inert when disabled, optional
/// accent tint for toggles (show-hidden) and the danger tint for Delete.
fn dock_button(
    ui: &mut Ui,
    glyph: char,
    tint: Color32,
    tip: &str,
    width: f32,
    enabled: bool,
) -> egui::Response {
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 24.0), sense);
    let p = ui.painter();
    let hovered = resp.hovered();
    if hovered {
        p.rect_filled(rect.shrink(1.0), 5.0, theme::ACCENT_SOFT());
    }
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        theme::icon(13.0),
        if enabled { tint } else { theme::TEXT_FAINT() },
    );
    let resp = resp.on_hover_text(if enabled {
        tip.to_string()
    } else {
        format!("{tip} — nothing selected yet, right-click rows to select")
    });
    resp
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
