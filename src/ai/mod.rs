//! AI Infrastructure Copilot: a natural-language control layer on top of
//! Culvert's existing SSH, tunnel, SFTP and file-browser machinery.
//!
//! Architecture rule (non-negotiable): **the model decides, the application
//! executes.** The engine task only ever
//!   * talks to LLM providers over the provider-agnostic [`provider`] layer,
//!   * runs *remote* read tools through the same `ssh`/`russh-sftp` layers the
//!     rest of the app uses, and
//!   * *requests* state-changing work from the UI thread via [`UiAction`]s,
//!     which `app.rs` executes with its existing code paths.
//!
//! Everything that passes from tool results to a model goes through the
//! [`security`] redaction layer and, when needed, an explicit user gate.

pub mod config;
pub mod engine;
pub mod hosts;
pub mod panel;
pub mod provider;
pub mod security;
pub mod tools;

pub use panel::AiPanel;

use crate::session::SshSession;
use std::sync::{Arc, Mutex, RwLock};

// ---------------------------------------------------------------------------
// UI context snapshot
// ---------------------------------------------------------------------------

/// State the app shares with the engine every frame. Split in two halves:
/// `profiles_summary`/`tunnels`/`dock`/… feed prompts and answers, while
/// `profiles` (with credentials) is used *only* by executors — prompts are
/// built exclusively from the summary fields.
#[derive(Clone, Default)]
pub struct UiContext {
    /// Session id of the focused shell tab (the app's notion of "current").
    pub active_session_id: Option<String>,
    /// Session ids that currently have an open shell window.
    pub open_session_ids: Vec<String>,
    /// The docked file browser: which server, where, what is selected.
    pub dock: Option<CtxDock>,
    /// Prompt-safe profile list.
    pub profiles_summary: Vec<CtxSession>,
    /// Every configured tunnel (bound to a session or standalone).
    pub tunnels: Vec<CtxTunnel>,
    /// Full profiles incl. credentials — executor use only, never in prompts.
    pub profiles: Vec<SshSession>,
}

#[derive(Clone)]
pub struct CtxSession {
    pub id: String,
    pub name: String,
    pub host: String,
    pub username: String,
    pub protocol: String,
    pub group: Option<String>,
}

#[derive(Clone)]
pub struct CtxDock {
    pub session_id: String,
    pub cwd: String,
    pub selected: Option<String>,
}

#[derive(Clone)]
pub struct CtxTunnel {
    /// `forward_key(session_id, forward_id)`.
    pub key: String,
    pub session_label: String,
    pub summary: String,
    pub running: bool,
    pub auto_reconnect: bool,
}

// ---------------------------------------------------------------------------
// UI actions (the engine asks, the app does)
// ---------------------------------------------------------------------------

/// Work that must run on the UI thread with the app's existing code paths.
/// The engine emits [`engine::AiEvent::Action`] and waits for
/// [`engine::AiCmd::ActionDone`].
#[derive(Clone, Debug)]
pub enum UiAction {
    /// Open a shell tab for this saved session (same as double-clicking it).
    OpenSession { session_id: String },
    /// Close an open shell tab for this session.
    CloseSession { session_id: String },
    /// Start a tunnel by `forward_key`.
    StartTunnel { key: String },
    /// Stop a tunnel by `forward_key`.
    StopTunnel { key: String },
    /// Point the docked file browser at a path (its session must be shown).
    DockNavigate { session_id: String, path: String },
    /// Type a command into the visible shell tab for `session_id` (opening the
    /// tab first if needed) and report the output once the shell prints the
    /// completion marker. Interactive: the user can answer sudo/password
    /// prompts and watch the command run.
    RunInTerminal {
        session_id: String,
        label: String,
        command: String,
    },
    /// Open a remote file in the local editor (download → edit → auto-sync).
    OpenFile { session_id: String, path: String },
}

// ---------------------------------------------------------------------------
// Audit trail
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct AuditEntry {
    /// Local wall-clock timestamp.
    pub time: String,
    /// request | tool | approval | sensitive | error | info
    pub kind: &'static str,
    pub detail: String,
}

pub type SharedAudit = Arc<Mutex<Vec<AuditEntry>>>;

pub fn new_audit() -> SharedAudit {
    Arc::new(Mutex::new(Vec::new()))
}

/// Append to the in-memory trail (shown in the panel) and to the on-disk
/// log (`%APPDATA%\culvert\ai-audit.log`) for durability.
pub fn push_audit(buf: &SharedAudit, kind: &'static str, detail: impl Into<String>) {
    let detail = detail.into();
    let time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    if let Ok(mut w) = buf.lock() {
        w.push(AuditEntry {
            time: time.clone(),
            kind,
            detail: detail.clone(),
        });
        if w.len() > 500 {
            let drop = w.len() - 500;
            w.drain(..drop);
        }
    }
    use std::io::Write;
    let path = config::audit_log_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{time} [{kind}] {detail}");
    }
}

/// New snapshot slot for the panel/engine pair.
pub fn new_context() -> Arc<RwLock<UiContext>> {
    Arc::new(RwLock::new(UiContext::default()))
}
