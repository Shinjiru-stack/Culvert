//! Application shell.
//!
//! Layout: a narrow sidebar with saved sessions (double-click opens a shell
//! tab), a tabbed workspace whose first tab is the tunnel manager, and a
//! one-line status bar. Everything is UI-driven — no commands anywhere.

use crate::log::{LogEntry, LogLevel, SharedLogs};
use crate::session::{
    load_groups, load_sessions, save_groups, save_sessions, AuthType, ForwardType, PortForward,
    SessionGroup, SshSession,
};
use crate::terminal::{TermStatus, TerminalSession};
use crate::theme;
use crate::tunnel::{forward_key, TunnelManager};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, Frame, Layout, Margin, RichText, Sense, Stroke, Ui,
};
use std::time::Duration;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub struct TunnelApp {
    sessions: Vec<SshSession>,
    /// Session buckets (Production, Staging, Home lab…). Sessions store the
    /// group id; groups live in their own file (order + dot color).
    groups: Vec<SessionGroup>,
    rt: tokio::runtime::Handle,
    logs: SharedLogs,
    tunnels: TunnelManager,

    tabs: Vec<Tab>,
    active: usize,

    selected: Option<String>,
    search: String,

    session_editor: Option<SessionEditor>,
    tunnel_editor: Option<TunnelEditor>,
    group_editor: Option<GroupEditor>,
    confirm: Option<Confirm>,
    toast: Option<Toast>,
    show_logs: bool,
    now: f64,
    mark_texture: Option<egui::TextureHandle>,
}

enum Tab {
    Tunnels,
    Terminal(Box<TerminalSession>),
}

#[derive(Clone, Copy, PartialEq)]
enum ToastKind {
    Info,
    Success,
    Error,
}

struct Toast {
    kind: ToastKind,
    text: String,
    born: f64,
}

struct SessionEditor {
    id: Option<String>,
    name: String,
    group: Option<String>,
    host: String,
    port: String,
    username: String,
    auth_type: AuthType,
    password: String,
    key_path: String,
    key_passphrase: String,
    error: Option<String>,
}

impl SessionEditor {
    fn new() -> Self {
        Self {
            id: None,
            name: String::new(),
            group: None,
            host: String::new(),
            port: "22".to_string(),
            username: String::new(),
            auth_type: AuthType::Password,
            password: String::new(),
            key_path: String::new(),
            key_passphrase: String::new(),
            error: None,
        }
    }

    fn from(s: &SshSession) -> Self {
        Self {
            id: Some(s.id.clone()),
            name: s.name.clone(),
            group: s.group.clone(),
            host: s.host.clone(),
            port: s.port.to_string(),
            username: s.username.clone(),
            auth_type: s.auth_type.clone(),
            password: s.password.clone(),
            key_path: s.key_path.clone(),
            key_passphrase: s.key_passphrase.clone(),
            error: None,
        }
    }
}

struct TunnelEditor {
    forward_id: Option<String>,
    session_id: String,
    fwd_type: ForwardType,
    listen_host: String,
    listen_port: String,
    dest_host: String,
    dest_port: String,
    auto_reconnect: bool,
    error: Option<String>,
}

impl TunnelEditor {
    fn new(session_id: String) -> Self {
        Self {
            forward_id: None,
            session_id,
            fwd_type: ForwardType::Local,
            listen_host: "127.0.0.1".to_string(),
            listen_port: "8080".to_string(),
            dest_host: "127.0.0.1".to_string(),
            dest_port: "80".to_string(),
            auto_reconnect: false,
            error: None,
        }
    }

    fn from(session_id: &str, f: &PortForward) -> Self {
        Self {
            forward_id: Some(f.id.clone()),
            session_id: session_id.to_string(),
            fwd_type: f.fwd_type.clone(),
            listen_host: f.listen_host.clone(),
            listen_port: f.listen_port.to_string(),
            dest_host: f.dest_host.clone(),
            dest_port: f.dest_port.to_string(),
            auto_reconnect: f.auto_reconnect,
            error: None,
        }
    }
}

/// Group create/rename editor. `apply_to` optionally carries a session id
/// ("move this session into the created group" flow).
struct GroupEditor {
    id: Option<String>,
    name: String,
    color: Option<[u8; 3]>,
    apply_to: Option<String>,
    error: Option<String>,
}

impl GroupEditor {
    fn new() -> Self {
        Self {
            id: None,
            name: String::new(),
            color: None,
            apply_to: None,
            error: None,
        }
    }

    fn for_session(session_id: &str) -> Self {
        Self {
            apply_to: Some(session_id.to_string()),
            ..Self::new()
        }
    }

    fn from(g: &SessionGroup) -> Self {
        Self {
            id: Some(g.id.clone()),
            name: g.name.clone(),
            color: g.color,
            apply_to: None,
            error: None,
        }
    }
}

enum Confirm {
    Session { id: String, name: String },
    Tunnel {
        key: String,
        session_id: String,
        forward_id: String,
        label: String,
    },
    Group { id: String, name: String },
}

#[derive(Clone, PartialEq)]
enum CardAction {
    Select,
    Connect,
    Edit,
    Duplicate,
    Delete,
    CopyCommand,
    /// Move the session into the group with this id; empty string = ungroup.
    MoveToGroup(String),
    /// Open the group editor pre-targeted at this session: saving it creates
    /// the group AND moves the session in.
    NewGroup(String),
}

/// Row-owned actions; the "session_id"-slash-"forward_id" pair is carried
/// outside (see the tunnels page action plumbing).
#[derive(Clone, Copy)]
enum RowAction {
    Toggle,
    ToggleAuto,
    Edit,
    Delete,
}

#[derive(Clone, Copy, PartialEq)]
enum GroupCmd {
    Toggle,
    Edit,
    Delete,
}

/// Chrome-style tab context menu (right-click a chip), resolved against the
/// tab index it was opened on.
#[derive(Clone, Copy, PartialEq)]
enum TabMenu {
    CloseSelf,
    CloseOthers,
    CloseRight,
    CloseAll,
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl TunnelApp {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        rt: tokio::runtime::Handle,
        logs: SharedLogs,
    ) -> Self {
        Self::new_inner(rt, logs)
    }

    fn new_inner(rt: tokio::runtime::Handle, logs: SharedLogs) -> Self {
        let sessions = load_sessions();
        let groups = load_groups();
        let selected = sessions.first().map(|s| s.id.clone());
        Self {
            sessions,
            groups,
            tunnels: TunnelManager::new(rt.clone(), logs.clone()),
            rt,
            logs,
            tabs: vec![Tab::Tunnels],
            active: 0,
            selected,
            search: String::new(),
            session_editor: None,
            tunnel_editor: None,
            confirm: None,
            toast: None,
            show_logs: false,
            group_editor: None,
            now: 0.0,
            mark_texture: None,
        }
    }

    fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        self.toast = Some(Toast {
            kind,
            text: text.into(),
            born: self.now,
        });
    }

    fn find_tab_for_session(&self, session_id: &str) -> Option<usize> {
        self.tabs.iter().position(|t| match t {
            Tab::Terminal(term) => term.session_id == session_id,
            Tab::Tunnels => false,
        })
    }

    fn open_terminal(&mut self, ctx: &egui::Context, session_id: &str) {
        if let Some(i) = self.find_tab_for_session(session_id) {
            self.active = i;
            return;
        }
        let Some(session) = self
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .cloned()
        else {
            return;
        };
        if let Err(e) = session.validate() {
            self.toast(ToastKind::Error, format!("Cannot open shell: {e}"));
            return;
        }
        let term = TerminalSession::spawn(ctx, self.rt.clone(), self.logs.clone(), session);
        self.tabs.push(Tab::Terminal(Box::new(term)));
        self.active = self.tabs.len() - 1;
    }

    fn close_tab(&mut self, index: usize) {
        if index == 0 || index >= self.tabs.len() {
            return;
        }
        if let Tab::Terminal(t) = &self.tabs[index] {
            t.close();
        }
        self.tabs.remove(index);
        if self.active == index {
            self.active = index.saturating_sub(1);
        } else if self.active > index {
            self.active -= 1;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
    }

    /// Chrome-style close operations. Iterating from the end keeps the
    /// earlier indices stable while tabs shift.
    fn close_right_tabs(&mut self, from: usize) {
        for i in (from + 1..self.tabs.len()).rev() {
            self.close_tab(i);
        }
    }

    fn close_other_tabs(&mut self, keep: usize) {
        for i in (1..self.tabs.len()).rev() {
            if i != keep {
                self.close_tab(i);
            }
        }
    }

    fn close_all_terminal_tabs(&mut self) {
        for i in (1..self.tabs.len()).rev() {
            self.close_tab(i);
        }
    }

    /// Flip the auto-reconnect switch of one tunnel (persists the field and
    /// applies it live to a running instance).
    fn toggle_tunnel_auto(&mut self, session_id: &str, forward_id: &str) {
        let key = forward_key(session_id, forward_id);
        let found = self
            .sessions
            .iter_mut()
            .find(|s| s.id == session_id)
            .and_then(|s| s.forwards.iter_mut().find(|f| f.id == forward_id));
        let Some(f) = found else { return };
        f.auto_reconnect = !f.auto_reconnect;
        let (on, desc) = (f.auto_reconnect, forward_desc(f));
        if self.tunnels.is_running(&key) {
            self.tunnels.set_auto_reconnect(&key, on);
            self.toast(
                ToastKind::Info,
                format!("{desc} — auto-reconnect {} for the running tunnel", if on { "enabled" } else { "disabled" }),
            );
        }
        self.persist(if on { "Auto-reconnect enabled" } else { "Auto-reconnect disabled" });
    }

    /// Move a session into a group (empty target = ungroup).
    fn move_session_to_group(&mut self, session_id: &str, group_id: &str) {
        let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) else {
            return;
        };
        let moved_to = if group_id.is_empty() {
            "No group".to_string()
        } else {
            match self.groups.iter().find(|g| g.id == group_id) {
                Some(g) => g.name.clone(),
                None => "a group".to_string(),
            }
        };
        s.group = if group_id.is_empty() {
            None
        } else {
            Some(group_id.to_string())
        };
        self.persist(format!("Session moved to {moved_to}"));
    }

    fn toggle_tunnel(&mut self, session_id: &str, forward_id: &str) {
        let key = forward_key(session_id, forward_id);
        if self.tunnels.is_running(&key) {
            self.tunnels.stop(&key);
            self.toast(ToastKind::Info, "Tunnel stopped");
            return;
        }
        let found = self
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .and_then(|s| {
                s.forwards
                    .iter()
                    .find(|f| f.id == forward_id)
                    .map(|f| (s.clone(), f.clone()))
            });
        let Some((session, forward)) = found else { return };
        match self.tunnels.start(&session, &forward) {
            Ok(()) => self.toast(
                ToastKind::Success,
                format!("Tunnel started · {}", forward_desc(&forward)),
            ),
            Err(e) => self.toast(ToastKind::Error, e),
        }
    }

    fn persist(&mut self, ok_msg: impl Into<String>) {
        match save_sessions(&self.sessions) {
            Ok(_) => self.toast(ToastKind::Success, ok_msg),
            Err(e) => self.toast(ToastKind::Error, format!("Could not save sessions: {e}")),
        }
    }

    fn save_session_editor(&mut self) {
        let Some(ed) = &mut self.session_editor else {
            return;
        };
        let port: u16 = match ed.port.trim().parse::<u16>() {
            Ok(p) if p > 0 => p,
            _ => {
                ed.error = Some("Port must be a number between 1 and 65535".to_string());
                return;
            }
        };
        let mut session = match &ed.id {
            Some(id) => self
                .sessions
                .iter()
                .find(|s| &s.id == id)
                .cloned()
                .unwrap_or_default(),
            None => SshSession::default(),
        };
        session.name = ed.name.trim().to_string();
        session.group = ed.group.clone();
        session.host = ed.host.trim().to_string();
        session.port = port;
        session.username = ed.username.trim().to_string();
        session.auth_type = ed.auth_type.clone();
        session.password = ed.password.clone();
        session.key_path = ed.key_path.clone();
        session.key_passphrase = ed.key_passphrase.clone();

        if let Err(e) = session.validate() {
            ed.error = Some(e);
            return;
        }
        let is_new = ed.id.is_none();
        let id = session.id.clone();
        match self.sessions.iter().position(|s| s.id == id) {
            Some(i) => self.sessions[i] = session,
            None => self.sessions.push(session),
        }
        self.selected = Some(id);
        self.session_editor = None;
        self.persist(if is_new { "Session added" } else { "Session saved" });
    }

    fn save_tunnel_editor(&mut self) {
        let Some(ed) = &mut self.tunnel_editor else {
            return;
        };
        let listen_port: u16 = match ed.listen_port.trim().parse::<u16>() {
            Ok(p) if p > 0 => p,
            _ => {
                ed.error = Some("Listen port must be a number between 1 and 65535".to_string());
                return;
            }
        };
        let dest_port: u16 = match ed.fwd_type {
            ForwardType::Dynamic => 0,
            _ => match ed.dest_port.trim().parse::<u16>() {
                Ok(p) if p > 0 => p,
                _ => {
                    ed.error =
                        Some("Destination port must be a number between 1 and 65535".to_string());
                    return;
                }
            },
        };
        let mut forward = PortForward {
            id: ed
                .forward_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            fwd_type: ed.fwd_type.clone(),
            listen_host: if ed.listen_host.trim().is_empty() {
                "127.0.0.1".to_string()
            } else {
                ed.listen_host.trim().to_string()
            },
            listen_port,
            dest_host: ed.dest_host.trim().to_string(),
            dest_port,
            auto_reconnect: ed.auto_reconnect,
        };
        if forward.dest_host.is_empty() && forward.fwd_type != ForwardType::Dynamic {
            forward.dest_host = "127.0.0.1".to_string();
        }
        if let Err(e) = forward.validate() {
            ed.error = Some(e);
            return;
        }
        let Some(session) = self.sessions.iter_mut().find(|s| s.id == ed.session_id) else {
            ed.error = Some("Choose a session first".to_string());
            return;
        };
        match session
            .forwards
            .iter()
            .position(|f| f.id == forward.id)
        {
            Some(i) => session.forwards[i] = forward,
            None => session.forwards.push(forward),
        }
        // Apply live to a running tunnel: future drops honor the new flag.
        let key = forward_key(&ed.session_id, &ed.forward_id.clone().unwrap_or_default());
        if ed.forward_id.is_some() && self.tunnels.is_running(&key) {
            self.tunnels.set_auto_reconnect(&key, ed.auto_reconnect);
        }
        self.tunnel_editor = None;
        self.persist("Tunnel saved");
    }

    /// Group editor plumbing -------------------------------------------------

    fn persist_groups(&mut self, ok_msg: &str) {
        match save_groups(&self.groups) {
            Ok(_) => self.toast(ToastKind::Success, ok_msg),
            Err(e) => self.toast(ToastKind::Error, format!("Could not save groups: {e}")),
        }
    }

    fn save_group_editor(&mut self) {
        let Some(mut ed) = self.group_editor.take() else {
            return;
        };
        let name = ed.name.trim().to_string();
        if name.is_empty() {
            ed.error = Some("Group name is required".to_string());
            self.group_editor = Some(ed);
            return;
        }
        if let Some(existing) = &ed.id {
            if let Some(g) = self.groups.iter_mut().find(|g| &g.id == existing) {
                g.name = name.clone();
                g.color = ed.color;
            }
            self.group_editor = None;
            self.persist_groups("Group saved");
        } else {
            let g = SessionGroup {
                id: uuid::Uuid::new_v4().to_string(),
                name,
                color: ed.color,
                expanded: true,
            };
            let (created_for, group_id) = (ed.apply_to.take(), g.id.clone());
            self.groups.push(g);
            self.group_editor = None;
            if let Some(sid) = created_for {
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    s.group = Some(group_id);
                }
                self.persist("Group created — session moved in");
            } else {
                self.toast(ToastKind::Success, "Group created");
            }
            if let Err(e) = save_groups(&self.groups) {
                self.toast(ToastKind::Error, format!("Could not save groups: {e}"));
            }
        }
    }

    fn delete_group(&mut self, id: &str) {
        self.groups.retain(|g| g.id != id);
        for s in &mut self.sessions {
            if s.group.as_deref() == Some(id) {
                s.group = None;
            }
        }
        self.group_editor = None;
        // Sessions (members unassigned) and groups both changed; save both.
        let g_ok = save_groups(&self.groups).is_ok();
        match save_sessions(&self.sessions) {
            Ok(_) if g_ok => {
                self.toast(ToastKind::Success, "Group deleted — sessions kept")
            }
            Ok(_) => self.toast(ToastKind::Error, "Could not save groups"),
            Err(e) => self.toast(ToastKind::Error, format!("Could not save sessions: {e}")),
        }
    }

    fn delete_session(&mut self, id: &str) {
        self.tunnels.stop_for_session(id);
        let mut i = self.tabs.len();
        while i > 0 {
            i -= 1;
            let matches_session =
                matches!(&self.tabs[i], Tab::Terminal(t) if t.session_id == id);
            if matches_session {
                self.close_tab(i);
            }
        }
        self.sessions.retain(|s| s.id != id);
        if self.selected.as_deref() == Some(id) {
            self.selected = self.sessions.first().map(|s| s.id.clone());
        }
        self.persist("Session deleted");
    }

    fn delete_tunnel(&mut self, session_id: &str, forward_id: &str) {
        let key = forward_key(session_id, forward_id);
        self.tunnels.stop(&key);
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.forwards.retain(|f| f.id != forward_id);
        }
        self.persist("Tunnel deleted");
    }

    fn duplicate_session(&mut self, id: &str) {
        let Some(orig) = self.sessions.iter().find(|s| &s.id == id).cloned() else {
            return;
        };
        let mut copy = orig;
        copy.id = uuid::Uuid::new_v4().to_string();
        copy.name = format!("{} copy", copy.name);
        for f in &mut copy.forwards {
            f.id = uuid::Uuid::new_v4().to_string();
        }
        self.selected = Some(copy.id.clone());
        self.sessions.push(copy);
        self.persist("Session duplicated");
    }

    fn terminal_focused(&self, ctx: &egui::Context) -> bool {
        self.tabs.iter().any(|t| match t {
            Tab::Terminal(term) => term.is_focused(ctx),
            Tab::Tunnels => false,
        })
    }
}

// ---------------------------------------------------------------------------
// eframe entry
// ---------------------------------------------------------------------------

impl eframe::App for TunnelApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.now = ctx.input(|i| i.time);
        self.ui(ctx);
    }
}

impl TunnelApp {
    pub fn ui(&mut self, ctx: &egui::Context) {
        // Drain async terminal events first so this frame renders reality.
        let mut any_connecting = false;
        for tab in self.tabs.iter_mut() {
            if let Tab::Terminal(t) = tab {
                t.poll();
                if matches!(t.status, TermStatus::Connecting) {
                    any_connecting = true;
                }
            }
        }

        // Terminal overlay buttons.
        let mut close_idx: Option<usize> = None;
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if let Tab::Terminal(t) = tab {
                if t.want_reconnect {
                    t.reconnect();
                    any_connecting = true;
                }
                if t.want_close {
                    close_idx = Some(i);
                }
            }
        }
        if let Some(i) = close_idx {
            self.close_tab(i);
        }

        // Global shortcuts (never while a shell has the keyboard).
        if !self.terminal_focused(ctx) {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::N)) {
                self.session_editor = Some(SessionEditor::new());
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::T)) {
                if !self.sessions.is_empty() {
                    let id = self
                        .selected
                        .clone()
                        .unwrap_or_else(|| self.sessions[0].id.clone());
                    self.tunnel_editor = Some(TunnelEditor::new(id));
                }
            }
        }

        egui::TopBottomPanel::bottom("statusbar")
            .exact_height(30.0)
            .frame(
                Frame::NONE
                    .fill(theme::BG_PANEL_ALT)
                    .stroke(Stroke::new(1.0_f32, theme::BORDER)),
            )
            .show(ctx, |ui| self.statusbar(ui));

        egui::SidePanel::left("sessions")
            .exact_width(266.0)
            .resizable(false)
            .frame(
                Frame::NONE
                    .fill(theme::BG_PANEL)
                    .inner_margin(Margin {
                        left: 14,
                        right: 14,
                        top: 14,
                        bottom: 10,
                    }),
            )
            .show(ctx, |ui| self.sidebar(ui, ctx));

        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(theme::BG_WINDOW).inner_margin(Margin::symmetric(16, 10)))
            .show(ctx, |ui| {
                self.tab_strip(ui);
                ui.add_space(6.0);
                self.workspace(ui, ctx);
            });

        self.session_editor_modal(ctx);
        self.tunnel_editor_modal(ctx);
        self.group_editor_modal(ctx);
        self.confirm_modal(ctx);
        self.logs_modal(ctx);
        self.toast_ui(ctx);

        if any_connecting {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    // -----------------------------------------------------------------------
    // Sidebar: saved sessions
    // -----------------------------------------------------------------------

    fn sidebar(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let mark_texture = self
            .mark_texture
            .get_or_insert_with(|| theme::brand_texture(ctx, 128))
            .clone();
        ui.horizontal(|ui| {
            paint_mark(ui, &mark_texture, 22.0);
            ui.add_space(2.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("Culvert").size(14.5).strong());
                ui.label(
                    RichText::new(format!("v{} · open source", env!("CARGO_PKG_VERSION")))
                        .size(10.5)
                        .color(theme::TEXT_FAINT),
                );
            });
        });
        ui.add_space(4.0);
        ui.label(
            RichText::new("Double-click a session to open a shell")
                .size(11.0)
                .color(theme::TEXT_FAINT),
        );
        ui.add_space(10.0);

        let full = egui::vec2(ui.available_width(), 31.0);
        if styled_button(
            ui,
            RichText::new("＋  New session").size(13.5),
            true,
            false,
            Some(full),
        )
        .clicked()
        {
            self.session_editor = Some(SessionEditor::new());
        }
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Sessions").text_style(theme::h2()).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ghost_button(
                    ui,
                    RichText::new("＋ Group").size(11.5).color(theme::TEXT_DIM),
                    false,
                )
                .on_hover_text("Create a group to organize sessions (Prod, Staging, Home…)")
                .clicked()
                {
                    self.group_editor = Some(GroupEditor::new());
                }
            });
        });
        ui.add_space(6.0);

        if self.sessions.len() > 6 {
            ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .hint_text("Search sessions…")
                    .desired_width(ui.available_width())
                    .margin(egui::vec2(8.0, 5.0)),
            );
            ui.add_space(6.0);
        }

        let filter = self.search.trim().to_lowercase();
        let ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| {
                filter.is_empty()
                    || s.name.to_lowercase().contains(&filter)
                    || s.host.to_lowercase().contains(&filter)
                    || s.username.to_lowercase().contains(&filter)
            })
            .map(|s| s.id.clone())
            .collect();

        // Snapshot for rendering; toggles applied after the frame.
        let groups: Vec<SessionGroup> = self.groups.clone();

        let mut card_actions: Vec<(String, CardAction)> = Vec::new();
        let mut group_cmds: Vec<(String, GroupCmd)> = Vec::new();

        egui::ScrollArea::vertical()
            .id_salt("session_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.sessions.is_empty() {
                    empty_hint(
                        ui,
                        "No sessions yet",
                        "Click “New session” to save your first SSH host. Tunnels and shells both start from here.",
                    );
                    return;
                }
                if ids.is_empty() {
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new("Nothing matches your search.")
                            .size(12.0)
                            .color(theme::TEXT_DIM),
                    );
                    return;
                }

                if groups.is_empty() {
                    // Flat classic list when the user hasn't grouped anything.
                    for id in &ids {
                        self.push_session_card(ui, ctx, id, &mut card_actions);
                    }
                } else {
                    for g in &groups {
                        let members: Vec<String> = ids
                            .iter()
                            .filter(|id| {
                                self.sessions
                                    .iter()
                                    .find(|s| s.id == **id)
                                    .map(|s| s.group.as_deref() == Some(g.id.as_str()))
                                    .unwrap_or(false)
                            })
                            .cloned()
                            .collect();
                        if members.is_empty() {
                            continue; // filtered out / empty groups hide away
                        }
                        let cmd = group_header_row(
                            ui,
                            &g.name,
                            g.color.map(|c| Color32::from_rgb(c[0], c[1], c[2])),
                            g.expanded,
                            members.len(),
                        );
                        if let Some(cmd) = cmd {
                            group_cmds.push((g.id.clone(), cmd));
                        }
                        if g.expanded {
                            for id in &members {
                                self.push_session_card(ui, ctx, id, &mut card_actions);
                            }
                            ui.add_space(8.0);
                        }
                    }
                    let ungrouped: Vec<String> = ids
                        .iter()
                        .filter(|id| {
                            self.sessions.iter().find(|s| s.id == **id).map(|s| {
                                match &s.group {
                                    None => true,
                                    Some(gid) => !groups.iter().any(|g| &g.id == gid),
                                }
                            }).unwrap_or(false)
                        })
                        .cloned()
                        .collect();
                    if !ungrouped.is_empty() {
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new("No group")
                                .size(11.0)
                                .color(theme::TEXT_FAINT),
                        );
                        ui.add_space(2.0);
                        for id in &ungrouped {
                            self.push_session_card(ui, ctx, id, &mut card_actions);
                        }
                    }
                }
            });

        for (gid, cmd) in group_cmds {
            match cmd {
                GroupCmd::Toggle => {
                    if let Some(g) = self.groups.iter_mut().find(|g| g.id == gid) {
                        g.expanded = !g.expanded;
                    }
                }
                GroupCmd::Edit => {
                    if let Some(g) = self.groups.iter().find(|g| g.id == gid) {
                        self.group_editor = Some(GroupEditor::from(g));
                    }
                }
                GroupCmd::Delete => {
                    if let Some(g) = self.groups.iter().find(|g| g.id == gid) {
                        self.confirm = Some(Confirm::Group {
                            id: g.id.clone(),
                            name: g.name.clone(),
                        });
                    }
                }
            }
        }
        for (id, action) in card_actions {
            match action {
                CardAction::Select => self.selected = Some(id.clone()),
                CardAction::Connect => self.open_terminal(ctx, &id),
                CardAction::Edit => {
                    if let Some(s) = self.sessions.iter().find(|s| s.id == id) {
                        self.session_editor = Some(SessionEditor::from(s));
                    }
                }
                CardAction::Duplicate => self.duplicate_session(&id),
                CardAction::Delete => {
                    if let Some(s) = self.sessions.iter().find(|s| s.id == id) {
                        self.confirm = Some(Confirm::Session {
                            id: id.clone(),
                            name: s.name.clone(),
                        });
                    }
                }
                CardAction::CopyCommand => {
                    if let Some(s) = self.sessions.iter().find(|s| s.id == id) {
                        ctx.copy_text(ssh_command(s));
                        self.toast(ToastKind::Info, "SSH command copied");
                    }
                }
                CardAction::MoveToGroup(target) => self.move_session_to_group(&id, &target),
                CardAction::NewGroup(sid) => {
                    self.group_editor = Some(GroupEditor::for_session(&sid));
                }
            }
        }
    }

    /// Renders one session card for `id`, queuing its action for the caller.
    fn push_session_card(
        &mut self,
        ui: &mut Ui,
        _ctx: &egui::Context,
        id: &str,
        out: &mut Vec<(String, CardAction)>,
    ) {
        let Some(session) = self.sessions.iter().find(|s| s.id == id) else {
            return;
        };
        let selected = self.selected.as_deref() == Some(id);
        let tunnel_live = self.tunnels.session_has_active(id);
        let tab_open = self.find_tab_for_session(id).is_some();
        let dot = self.group_dot_color(&session.group.clone().unwrap_or_default());
        let auth_label = match session.auth_type {
            AuthType::Password => "Password",
            AuthType::Key => "Key",
        };
        let groups_menu: Vec<(String, String)> = self
            .groups
            .iter()
            .filter(|g| Some(g.id.as_str()) != session.group.as_deref())
            .map(|g| (g.id.clone(), g.name.clone()))
            .collect();
        let action = session_card(
            ui,
            id,
            selected,
            &session.name,
            &session.connection_label(),
            auth_label,
            tunnel_live,
            tab_open,
            dot,
            &groups_menu,
        );
        if let Some(a) = action {
            out.push((id.to_string(), a));
        }
    }

    fn group_dot_color(&self, session_group: &str) -> Option<Color32> {
        let gid: Option<&str> = if session_group.is_empty() {
            None
        } else {
            Some(session_group)
        };
        self.groups.iter().find(|g| Some(g.id.as_str()) == gid).and_then(|g| g.color).map(|c| Color32::from_rgb(c[0], c[1], c[2]))
    }

    // -----------------------------------------------------------------------
    // Tab strip
    // -----------------------------------------------------------------------

    fn tab_strip(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let active = self.active == 0;
            let (clicked, _, menu) = tab_chip(ui, "Tunnels", None, active, false);
            if clicked {
                self.active = 0;
            }
            match menu {
                Some(TabMenu::CloseOthers) => self.close_other_tabs(0),
                Some(TabMenu::CloseRight) | Some(TabMenu::CloseAll) => {
                    self.close_all_terminal_tabs()
                }
                _ => {}
            }
            let mut activate: Option<usize> = None;
            let mut close: Option<usize> = None;
            let mut menu_cmd: Option<(usize, TabMenu)> = None;
            for (i, tab) in self.tabs.iter().enumerate().skip(1) {
                if let Tab::Terminal(t) = tab {
                    let dot = match &t.status {
                        TermStatus::Connected => Some(theme::SUCCESS),
                        TermStatus::Connecting => Some(theme::WARNING),
                        TermStatus::Closed(_) => Some(theme::DANGER),
                    };
                    let (clicked, closed, menu) =
                        tab_chip(ui, &t.title, dot, self.active == i, true);
                    if clicked {
                        activate = Some(i);
                    }
                    if closed {
                        close = Some(i);
                    }
                    if let Some(m) = menu {
                        menu_cmd = Some((i, m));
                    }
                }
            }
            if let Some(i) = activate {
                self.active = i;
            }
            if let Some(i) = close {
                self.close_tab(i);
            }
            if let Some((i, m)) = menu_cmd {
                match m {
                    TabMenu::CloseSelf => self.close_tab(i),
                    TabMenu::CloseRight => self.close_right_tabs(i),
                    TabMenu::CloseOthers => self.close_other_tabs(i),
                    TabMenu::CloseAll => self.close_all_terminal_tabs(),
                }
            }
        });
    }

    // -----------------------------------------------------------------------
    // Workspace
    // -----------------------------------------------------------------------

    fn workspace(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let active = self.active;
        if let Some(Tab::Terminal(term)) = self.tabs.get_mut(active) {
            term.ui(ui);
            return;
        }
        self.tunnels_page(ui, ctx);
    }

    fn tunnels_page(&mut self, ui: &mut Ui, _ctx: &egui::Context) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(
                    RichText::new("Port forwarding")
                        .text_style(theme::h1())
                        .strong(),
                );
                ui.label(
                    RichText::new(
                        "Tunnels run over any saved session — pick one, choose a type, fill in the ports. No commands.",
                    )
                    .size(12.5)
                    .color(theme::TEXT_DIM),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let enabled = !self.sessions.is_empty();
                let new_tunnel = styled_button(
                    ui,
                    RichText::new("＋  New tunnel").size(13.5),
                    enabled,
                    false,
                    None,
                );
                if new_tunnel.clicked() {
                    let id = self
                        .selected
                        .clone()
                        .filter(|id| self.sessions.iter().any(|s| &s.id == id))
                        .unwrap_or_else(|| self.sessions[0].id.clone());
                    self.tunnel_editor = Some(TunnelEditor::new(id));
                }
                if !enabled {
                    new_tunnel.on_hover_text("Add a session first — tunnels need an SSH host.");
                }
            });
        });
        ui.add_space(12.0);

        if self.sessions.is_empty() {
            empty_hint(
                ui,
                "No sessions yet",
                "Add an SSH session in the sidebar, then create tunnels that run over it.",
            );
            return;
        }

        // Snapshot rows, render, then apply actions (keeps borrows simple).
        struct Row {
            session_id: String,
            session_name: String,
            conn: String,
            forward: PortForward,
            running: bool,
            reconnecting: bool,
            auth: &'static str,
        }
        let rows: Vec<Row> = {
            let tunnels = &self.tunnels;
            self.sessions
                .iter()
                .flat_map(|s| {
                    let session_id = s.id.clone();
                    let session_name = s.name.clone();
                    let conn = s.connection_label();
                    let auth = match s.auth_type {
                        AuthType::Password => "Password",
                        AuthType::Key => "Key",
                    };
                    s.forwards.iter().map(move |f| Row {
                        session_id: session_id.clone(),
                        session_name: session_name.clone(),
                        conn: conn.clone(),
                        forward: f.clone(),
                        running: tunnels.is_running(&forward_key(&session_id, &f.id)),
                        reconnecting: tunnels.is_reconnecting(&forward_key(&session_id, &f.id)),
                        auth,
                    })
                })
                .collect()
        };

        if rows.is_empty() {
            empty_hint(
                ui,
                "No tunnels yet",
                "Click “New tunnel” to forward a port over one of your sessions. Start and stop each tunnel any time.",
            );
            return;
        }

        let mut actions: Vec<(RowAction, String, String)> = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("tunnel_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for row in &rows {
                    if let Some(a) = tunnel_row(
                        ui,
                        &row.session_name,
                        &row.conn,
                        &row.forward,
                        row.running,
                        row.reconnecting,
                        row.auth,
                    ) {
                        actions.push((a, row.session_id.clone(), row.forward.id.clone()));
                    }
                }
                ui.add_space(12.0);
            });

        for (action, session_id, forward_id) in actions {
            match action {
                RowAction::Toggle => self.toggle_tunnel(&session_id, &forward_id),
                RowAction::ToggleAuto => self.toggle_tunnel_auto(&session_id, &forward_id),
                RowAction::Edit => {
                    if let Some(f) = self
                        .sessions
                        .iter()
                        .find(|s| s.id == session_id)
                        .and_then(|s| s.forwards.iter().find(|f| f.id == forward_id))
                    {
                        self.tunnel_editor =
                            Some(TunnelEditor::from(&session_id, f));
                    }
                }
                RowAction::Delete => {
                    let label = self
                        .sessions
                        .iter()
                        .find(|s| s.id == session_id)
                        .and_then(|s| s.forwards.iter().find(|f| f.id == forward_id))
                        .map(forward_desc)
                        .unwrap_or_default();
                    self.confirm = Some(Confirm::Tunnel {
                        key: forward_key(&session_id, &forward_id),
                        session_id,
                        forward_id,
                        label,
                    });
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Modals
    // -----------------------------------------------------------------------

    fn session_editor_modal(&mut self, ctx: &egui::Context) {
        let Some(mut ed) = self.session_editor.take() else {
            return;
        };
        let mut keep = true;
        let mut save = false;
        let is_new = ed.id.is_none();
        let resp = egui::Modal::new(egui::Id::new("session_editor")).show(ctx, |ui| {
            ui.set_width(430.0);
            ui.label(
                RichText::new(if is_new { "New session" } else { "Edit session" })
                    .size(17.0)
                    .strong(),
            );
            ui.label(
                RichText::new("Saved locally. Used for shells and tunnels alike.")
                    .size(11.5)
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(12.0);

            field(ui, "Name", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut ed.name)
                        .hint_text("prod web server")
                        .desired_width(ui.available_width())
                        .margin(egui::vec2(8.0, 6.0)),
                );
            });
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 74.0).max(120.0);
                ui.vertical(|ui| {
                    field(ui, "Host", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut ed.host)
                                .hint_text("10.0.0.5 or server.example.com")
                                .desired_width(w)
                                .margin(egui::vec2(8.0, 6.0)),
                        );
                    });
                });
                field(ui, "Port", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut ed.port)
                            .desired_width(62.0)
                            .margin(egui::vec2(8.0, 6.0)),
                    );
                });
            });
            field(ui, "Username", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut ed.username)
                        .hint_text("root")
                        .desired_width(ui.available_width())
                        .margin(egui::vec2(8.0, 6.0)),
                );
            });

            field(ui, "Group (optional)", |ui| {
                let mut options: Vec<(String, String)> = self
                    .groups
                    .iter()
                    .map(|g| (g.id.clone(), g.name.clone()))
                    .collect();
                options.retain(|(id, _)| Some(id) != ed.group.as_ref());
                let selected_text = ed
                    .group
                    .as_deref()
                    .and_then(|gid| {
                        self.groups.iter().find(|g| g.id == gid).map(|g| g.name.clone())
                    })
                    .unwrap_or_else(|| "No group".to_string());
                egui::ComboBox::from_id_salt("session_group")
                    .selected_text(
                        RichText::new(selected_text).size(13.0).color(theme::TEXT),
                    )
                    .width(ui.available_width() - 2.0)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(ed.group.is_none(), "No group")
                            .clicked()
                        {
                            ed.group = None;
                        }
                        for (id, name) in options {
                            if ui
                                .selectable_label(ed.group.as_deref() == Some(id.as_str()), name)
                                .clicked()
                            {
                                ed.group = Some(id.clone());
                            }
                        }
                    });
            });

            ui.label(
                RichText::new("Authentication")
                    .size(12.0)
                    .color(theme::TEXT_DIM),
            );
            ui.horizontal(|ui| {
                if choice_pill(ui, "Password", ed.auth_type == AuthType::Password).clicked() {
                    ed.auth_type = AuthType::Password;
                    ed.error = None;
                }
                if choice_pill(ui, "Private key", ed.auth_type == AuthType::Key).clicked() {
                    ed.auth_type = AuthType::Key;
                    ed.error = None;
                }
            });
            ui.add_space(8.0);

            match ed.auth_type {
                AuthType::Password => {
                    field(ui, "Password", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut ed.password)
                                .password(true)
                                .desired_width(ui.available_width())
                                .margin(egui::vec2(8.0, 6.0)),
                        );
                    });
                }
                AuthType::Key => {
                    ui.horizontal(|ui| {
                        let w = (ui.available_width() - 92.0).max(140.0);
                        ui.vertical(|ui| {
                            field(ui, "Private key file", |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut ed.key_path)
                                        .hint_text("C:\\Users\\me\\.ssh\\id_ed25519")
                                        .desired_width(w)
                                        .margin(egui::vec2(8.0, 6.0)),
                                );
                            });
                        });
                        ui.add_space(4.0);
                        if ui.button("Browse…").clicked() {
                            if let Some(p) = rfd::FileDialog::new().pick_file() {
                                ed.key_path = p.display().to_string();
                            }
                        }
                    });
                    field(ui, "Key passphrase (optional)", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut ed.key_passphrase)
                                .password(true)
                                .desired_width(ui.available_width())
                                .margin(egui::vec2(8.0, 6.0)),
                        );
                    });
                }
            }

            if let Some(err) = &ed.error {
                ui.add_space(4.0);
                ui.label(RichText::new(err).size(12.0).color(theme::DANGER));
            }

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if styled_button(
                        ui,
                        RichText::new("Save").size(13.5).strong(),
                        true,
                        false,
                        None,
                    )
                    .clicked()
                    {
                        save = true;
                    }
                });
            });
        });
        if resp.should_close() {
            keep = false;
        }
        if save {
            self.session_editor = Some(ed);
            self.save_session_editor();
        } else if keep {
            self.session_editor = Some(ed);
        }
    }

    fn tunnel_editor_modal(&mut self, ctx: &egui::Context) {
        let Some(mut ed) = self.tunnel_editor.take() else {
            return;
        };
        let mut keep = true;
        let mut save = false;
        let is_new = ed.forward_id.is_none();
        let session_names: Vec<(String, String)> = self
            .sessions
            .iter()
            .map(|s| (s.id.clone(), format!("{} · {}", s.name, s.connection_label())))
            .collect();

        let resp = egui::Modal::new(egui::Id::new("tunnel_editor")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(
                RichText::new(if is_new { "New tunnel" } else { "Edit tunnel" })
                    .size(17.0)
                    .strong(),
            );
            ui.label(
                RichText::new("Forwards over a saved session. Runs in the background while you work.")
                    .size(11.5)
                    .color(theme::TEXT_FAINT),
            );
            ui.add_space(12.0);

            field(ui, "Via session", |ui| {
                let selected_label = session_names
                    .iter()
                    .find(|(id, _)| id == &ed.session_id)
                    .map(|(_, l)| l.clone())
                    .unwrap_or_else(|| "Choose a session…".to_string());
                egui::ComboBox::from_id_salt("tunnel_session")
                    .selected_text(selected_label)
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for (id, label) in &session_names {
                            ui.selectable_value(&mut ed.session_id, id.clone(), label);
                        }
                    });
            });

            ui.label(
                RichText::new("Type")
                    .size(12.0)
                    .color(theme::TEXT_DIM),
            );
            ui.horizontal(|ui| {
                for t in [ForwardType::Local, ForwardType::Remote, ForwardType::Dynamic] {
                    let active = ed.fwd_type == t;
                    if choice_pill(ui, t.as_str(), active).clicked() {
                        ed.fwd_type = t.clone();
                        ed.error = None;
                        match t {
                            ForwardType::Local => {
                                if ed.listen_host == "0.0.0.0" {
                                    ed.listen_host = "127.0.0.1".into();
                                }
                                if ed.listen_port == "9000" || ed.listen_port == "1080" {
                                    ed.listen_port = "8080".into();
                                }
                                if ed.dest_port == "3000" {
                                    ed.dest_port = "80".into();
                                }
                            }
                            ForwardType::Remote => {
                                if ed.listen_host == "127.0.0.1" {
                                    ed.listen_host = "0.0.0.0".into();
                                }
                                if ed.listen_port == "8080" || ed.listen_port == "1080" {
                                    ed.listen_port = "9000".into();
                                }
                                if ed.dest_port == "80" {
                                    ed.dest_port = "3000".into();
                                }
                            }
                            ForwardType::Dynamic => {
                                if ed.listen_host == "0.0.0.0" {
                                    ed.listen_host = "127.0.0.1".into();
                                }
                                if ed.listen_port == "8080" || ed.listen_port == "9000" {
                                    ed.listen_port = "1080".into();
                                }
                            }
                        }
                    }
                }
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(match ed.fwd_type {
                    ForwardType::Local => {
                        "Local (-L): expose a remote service on a local port, e.g. a database behind the server."
                    }
                    ForwardType::Remote => {
                        "Remote (-R): publish a service from this machine on the server — great for demos and webhooks."
                    }
                    ForwardType::Dynamic => {
                        "Dynamic (-D): start a local SOCKS5 proxy and route any app through the server."
                    }
                })
                .size(11.5)
                .color(theme::TEXT_FAINT),
            );
            ui.add_space(10.0);

            ui.horizontal(|ui| {
                let w = (ui.available_width() - 74.0).max(120.0);
                ui.vertical(|ui| {
                    field(
                        ui,
                        match ed.fwd_type {
                            ForwardType::Remote => "Listen host (on server)",
                            _ => "Listen host (this machine)",
                        },
                        |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut ed.listen_host)
                                    .desired_width(w)
                                    .margin(egui::vec2(8.0, 6.0)),
                            );
                        },
                    );
                });
                field(ui, "Port", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut ed.listen_port)
                            .desired_width(62.0)
                            .margin(egui::vec2(8.0, 6.0)),
                    );
                });
            });

            if ed.fwd_type != ForwardType::Dynamic {
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 74.0).max(120.0);
                    ui.vertical(|ui| {
                        field(
                            ui,
                            match ed.fwd_type {
                                ForwardType::Remote => "Forward to (this machine)",
                                _ => "Destination (from the server's view)",
                            },
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut ed.dest_host)
                                        .desired_width(w)
                                        .margin(egui::vec2(8.0, 6.0)),
                                );
                            },
                        );
                    });
                    field(ui, "Port", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut ed.dest_port)
                                .desired_width(62.0)
                                .margin(egui::vec2(8.0, 6.0)),
                        );
                    });
                });
            }

            ui.add_space(4.0);
            ui.checkbox(&mut ed.auto_reconnect, "Auto-reconnect if the tunnel drops")
                .on_hover_text("Watches the SSH connection and brings the tunnel back up automatically (2–15s backoff).");

            if let Some(err) = &ed.error {
                ui.add_space(4.0);
                ui.label(RichText::new(err).size(12.0).color(theme::DANGER));
            }

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if styled_button(
                        ui,
                        RichText::new("Save").size(13.5).strong(),
                        true,
                        false,
                        None,
                    )
                    .clicked()
                    {
                        save = true;
                    }
                });
            });
        });
        if resp.should_close() {
            keep = false;
        }
        if save {
            self.tunnel_editor = Some(ed);
            self.save_tunnel_editor();
        } else if keep {
            self.tunnel_editor = Some(ed);
        }
    }

    /// Create or rename a session group, optionally assigning a session.
    fn group_editor_modal(&mut self, ctx: &egui::Context) {
        let Some(mut ed) = self.group_editor.take() else {
            return;
        };
        let mut keep = true;
        let mut save = false;
        let is_new = ed.id.is_none();
        let apply_label = ed
            .apply_to
            .as_deref()
            .and_then(|sid| {
                self.sessions
                    .iter()
                    .find(|s| s.id == sid)
                    .map(|s| s.name.clone())
            })
            .unwrap_or_default();
        let swatches: [Option<Color32>; 8] = [
            None,
            Some(Color32::from_rgb(211, 47, 47)),
            Some(Color32::from_rgb(184, 110, 8)),
            Some(Color32::from_rgb(7, 133, 83)),
            Some(Color32::from_rgb(31, 103, 214)),
            Some(Color32::from_rgb(110, 58, 205)),
            Some(Color32::from_rgb(13, 148, 136)),
            Some(Color32::from_rgb(98, 105, 118)),
        ];
        let ed_ref = &mut ed;
        let resp = egui::Modal::new(egui::Id::new("group_editor")).show(ctx, |ui| {
            ui.set_width(360.0);
            ui.label(
                RichText::new(if is_new { "New group" } else { "Edit group" })
                    .size(17.0)
                    .strong(),
            );
            ui.label(
                RichText::new(if is_new {
                    if apply_label.is_empty() {
                        "Group sessions by environment, team — whatever works for you.".to_string()
                    } else {
                        format!("Group for “{apply_label}” — it will be moved in on save.")
                    }
                } else {
                    "Rename or recolor; sessions keep their membership.".to_string()
                })
                .size(11.5)
                .color(theme::TEXT_FAINT),
            );
            ui.add_space(12.0);
            field(ui, "Name", |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut ed_ref.name)
                        .hint_text("Production")
                        .desired_width(ui.available_width())
                        .margin(egui::vec2(8.0, 6.0)),
                );
            });
            field(ui, "Dot color", |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for (i, sw) in swatches.iter().enumerate() {
                        let selected = match (ed_ref.color, sw) {
                            (None, None) => true,
                            (Some(c), Some(s)) => c == [s.r(), s.g(), s.b()],
                            _ => false,
                        };
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(20.0, 20.0),
                            Sense::click(),
                        );
                        match sw {
                            Some(c) => {
                                ui.painter()
                                    .rect_filled(rect.shrink(2.0), 8.0, *c);
                            }
                            None => {
                                ui.painter()
                                    .rect_filled(rect.shrink(2.0), 8.0, theme::BG_PANEL_ALT);
                                ui.painter()
                                    .rect_stroke(rect.shrink(2.0), 8.0, Stroke::new(1.0_f32, theme::BORDER_STRONG), egui::StrokeKind::Inside);
                            }
                        }
                        if selected {
                            ui.painter()
                                .rect_stroke(rect, 9.0, Stroke::new(2.0_f32, theme::ACCENT), egui::StrokeKind::Inside);
                        }
                        if resp.clicked() {
                            ed_ref.color = sw.map(|c| [c.r(), c.g(), c.b()]);
                        }
                        if i == 0 {
                            ui.label(
                                RichText::new("(none) = gray-free")
                                    .size(10.5)
                                    .color(theme::TEXT_FAINT),
                            );
                        }
                    }
                });
            });
            if let Some(err) = &ed_ref.error {
                ui.label(RichText::new(err).size(12.0).color(theme::DANGER));
            }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if styled_button(ui, RichText::new("Save").size(13.5).strong(), true, false, None)
                        .clicked()
                    {
                        save = true;
                    }
                });
            });
        });
        if resp.should_close() {
            keep = false;
        }
        if save {
            self.group_editor = Some(ed);
            self.save_group_editor();
        } else if keep {
            self.group_editor = Some(ed);
        }
    }

    fn confirm_modal(&mut self, ctx: &egui::Context) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        let mut keep = true;
        let mut confirmed = false;
        let (title, body, ok_label, danger) = match &confirm {
            Confirm::Session { name, .. } => (
                format!("Delete “{name}”?"),
                "Its saved tunnels will be removed and running tunnels stopped.".to_string(),
                "Delete",
                true,
            ),
            Confirm::Tunnel { label, .. } => (
                "Delete this tunnel?".to_string(),
                format!("{label} — it will be stopped if running."),
                "Delete",
                true,
            ),
            Confirm::Group { name, .. } => (
                format!("Delete group “{name}”?"),
                "Sessions in the group stay — they just become ungrouped.".to_string(),
                "Delete group",
                true,
            ),
        };
        let resp = egui::Modal::new(egui::Id::new("confirm_delete")).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.label(RichText::new(title).size(16.0).strong());
            ui.add_space(4.0);
            ui.label(RichText::new(body).size(12.5).color(theme::TEXT_DIM));
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if styled_button(
                        ui,
                        RichText::new(ok_label).size(13.5).strong(),
                        true,
                        danger,
                        None,
                    )
                    .clicked()
                    {
                        confirmed = true;
                    }
                });
            });
        });
        if resp.should_close() {
            keep = false;
        }
        if confirmed {
            match confirm {
                Confirm::Session { id, .. } => self.delete_session(&id),
                Confirm::Tunnel {
                    key,
                    session_id,
                    forward_id,
                    ..
                } => {
                    let _ = key;
                    self.delete_tunnel(&session_id, &forward_id);
                }
                Confirm::Group { id, .. } => self.delete_group(&id),
            }
        } else if keep {
            self.confirm = Some(confirm);
        }
    }

    fn logs_modal(&mut self, ctx: &egui::Context) {
        if !self.show_logs {
            return;
        }
        let mut keep = true;
        let entries: Vec<LogEntry> = self
            .logs
            .lock()
            .map(|l| l.iter().rev().take(400).cloned().collect())
            .unwrap_or_default();
        let resp = egui::Modal::new(egui::Id::new("logs_modal")).show(ctx, |ui| {
            ui.set_width(640.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Activity").size(17.0).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Clear").clicked() {
                        if let Ok(mut l) = self.logs.lock() {
                            l.clear();
                        }
                    }
                });
            });
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if entries.is_empty() {
                        ui.label(
                            RichText::new("Nothing here yet.")
                                .size(12.0)
                                .color(theme::TEXT_DIM),
                        );
                    }
                    for e in &entries {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            ui.label(
                                RichText::new(&e.time)
                                    .size(11.0)
                                    .color(theme::TEXT_FAINT)
                                    .monospace(),
                            );
                            let (tag, color) = match e.level {
                                LogLevel::Info => ("info", theme::INFO),
                                LogLevel::Success => (" ok ", theme::SUCCESS),
                                LogLevel::Error => ("err ", theme::DANGER),
                            };
                            ui.label(RichText::new(tag).size(11.0).color(color).monospace());
                            ui.label(RichText::new(&e.session).size(11.5).strong());
                            ui.label(
                                RichText::new(&e.message)
                                    .size(12.0)
                                    .color(theme::TEXT_DIM),
                            );
                        });
                    }
                });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Close").clicked() {
                    keep = false;
                }
            });
        });
        if resp.should_close() {
            keep = false;
        }
        self.show_logs = keep;
    }

    // -----------------------------------------------------------------------
    // Status bar + toast
    // -----------------------------------------------------------------------

    fn statusbar(&mut self, ui: &mut Ui) {
        ui.horizontal_centered(|ui| {
            ui.add_space(10.0);
            let running = self.tunnels.running_count();
            let shells = self
                .tabs
                .iter()
                .filter(|t| matches!(t, Tab::Terminal(t) if matches!(t.status, TermStatus::Connected)))
                .count();
            let parts: Vec<String> = vec![
                format!(
                    "{} session{}",
                    self.sessions.len(),
                    if self.sessions.len() == 1 { "" } else { "s" }
                ),
                format!(
                    "{} tunnel{} running",
                    running,
                    if running == 1 { "" } else { "s" }
                ),
                format!(
                    "{} shell{}",
                    shells,
                    if shells == 1 { "" } else { "s" }
                ),
            ];
            ui.label(
                RichText::new(parts.join("  ·  "))
                    .size(11.5)
                    .color(theme::TEXT_DIM),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(10.0);
                ui.label(
                    RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                        .size(11.0)
                        .color(theme::TEXT_FAINT),
                );
                ui.add_space(8.0);
                if ui
                    .add(
                        egui::Button::new(RichText::new("Activity").size(11.5))
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::NONE),
                    )
                    .on_hover_text("Tunnel, shell and error log")
                    .clicked()
                {
                    self.show_logs = true;
                }
            });
        });
    }

    fn toast_ui(&mut self, ctx: &egui::Context) {
        let Some(t) = &self.toast else { return };
        let age = self.now - t.born;
        if age > 3.4 {
            self.toast = None;
            return;
        }
        let fade = if age > 2.7 { ((3.4 - age) / 0.7) as f32 } else { 1.0 };
        let (stroke_c, icon) = match t.kind {
            ToastKind::Info => (theme::INFO, "i"),
            ToastKind::Success => (theme::SUCCESS, "✓"),
            ToastKind::Error => (theme::DANGER, "!"),
        };
        let text = t.text.clone();
        egui::Area::new(egui::Id::new("toast"))
            .anchor(Align2::RIGHT_BOTTOM, egui::vec2(-14.0, -42.0))
            .interactable(false)
            .show(ctx, |ui| {
                Frame::NONE
                    .fill(with_alpha(theme::BG_CARD, fade))
                    .corner_radius(CornerRadius::same(10))
                    .stroke(Stroke::new(1.0_f32, with_alpha(stroke_c, fade)))
                    .inner_margin(Margin::symmetric(12, 9))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(icon)
                                    .size(12.0)
                                    .color(with_alpha(stroke_c, fade))
                                    .strong(),
                            );
                            ui.label(RichText::new(text).size(12.5).color(with_alpha(theme::TEXT, fade)));
                        });
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(80));
    }
}

// ---------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------

fn session_card(
    ui: &mut Ui,
    session_id: &str,
    selected: bool,
    name: &str,
    conn: &str,
    auth_label: &str,
    tunnel_live: bool,
    tab_open: bool,
    group_dot: Option<Color32>,
    groups_menu: &[(String, String)],
) -> Option<CardAction> {
    let mut action = None;
    let (a_fg, a_bg) = auth_palette(auth_label);
    let resp = Frame::NONE
        .fill(if selected {
            theme::BG_CARD_SEL
        } else {
            theme::BG_CARD
        })
        .corner_radius(CornerRadius::same(10))
        .stroke(Stroke::new(
            1.0_f32,
            if selected {
                theme::ACCENT
            } else {
                theme::BORDER
            },
        ))
        .inner_margin(Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                if let Some(c) = group_dot {
                    let (r, _) =
                        ui.allocate_exact_size(egui::vec2(9.0, 9.0), Sense::hover());
                    ui.painter().rect_filled(r, 3.0, c);
                }
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(RichText::new(name).size(13.5).color(theme::TEXT));
                        chip(ui, auth_label, a_fg, a_bg);
                    });
                    ui.label(
                        RichText::new(conn)
                            .size(11.5)
                            .color(theme::TEXT_DIM)
                            .monospace(),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if tunnel_live {
                        ui.label(RichText::new("●").size(9.0).color(theme::SUCCESS))
                            .on_hover_text("Tunnels running");
                    } else if tab_open {
                        ui.label(RichText::new("●").size(9.0).color(theme::ACCENT))
                            .on_hover_text("Shell open");
                    }
                });
            });
        })
        .response
        .interact(Sense::click());

    if resp.clicked() {
        action = Some(CardAction::Select);
    }
    if resp.double_clicked() {
        action = Some(CardAction::Connect);
    }
    let resp = resp.on_hover_text("Double-click to open a shell · right-click for more");
    resp.context_menu(|ui| {
        if ui.button("Open shell").clicked() {
            action = Some(CardAction::Connect);
            ui.close();
        }
        if ui.button("Edit session…").clicked() {
            action = Some(CardAction::Edit);
            ui.close();
        }
        if ui.button("Duplicate").clicked() {
            action = Some(CardAction::Duplicate);
            ui.close();
        }
        if ui.button("Copy SSH command").clicked() {
            action = Some(CardAction::CopyCommand);
            ui.close();
        }
        ui.separator();
        ui.label(
            RichText::new("Move to group").size(11.0).color(theme::TEXT_FAINT),
        );
        if ui.button("— No group —").clicked() {
            action = Some(CardAction::MoveToGroup(String::new()));
            ui.close();
        }
        if groups_menu.is_empty() {
            ui.label(
                RichText::new("(no groups yet — create one)").size(10.5).color(theme::TEXT_FAINT),
            );
        } else {
            for (gid, gname) in groups_menu {
                if ui.button(gname).clicked() {
                    action = Some(CardAction::MoveToGroup(gid.clone()));
                    ui.close();
                }
            }
        }
        if ui.button("＋ New group…").clicked() {
            action = Some(CardAction::NewGroup(session_id.to_string()));
            ui.close();
        }
        ui.separator();
        if ui.button("Delete").clicked() {
            action = Some(CardAction::Delete);
            ui.close();
        }
    });
    ui.add_space(6.0);
    action
}

/// Small collapsible group divider shown between session cards.
fn group_header_row(
    ui: &mut Ui,
    name: &str,
    color: Option<Color32>,
    expanded: bool,
    count: usize,
) -> Option<GroupCmd> {
    let mut cmd = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if let Some(c) = color {
            let (r, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), Sense::hover());
            ui.painter().rect_filled(r, 3.0, c);
        }
        let caret = if expanded { "▾" } else { "▸" };
        if ghost_button(
            ui,
            RichText::new(format!("{caret} {name}"))
                .size(12.0)
                .strong()
                .color(theme::TEXT_DIM),
            false,
        )
        .clicked()
        {
            cmd = Some(GroupCmd::Toggle);
        }
        ui.label(
            RichText::new(count.to_string())
                .size(10.5)
                .color(theme::TEXT_FAINT),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.menu_button(
                RichText::new("⋯").size(12.0).color(theme::TEXT_FAINT),
                |menu| {
                    if menu.button("Edit group").clicked() {
                        cmd = Some(GroupCmd::Edit);
                        menu.close();
                    }
                    if menu.button("Delete group").clicked() {
                        cmd = Some(GroupCmd::Delete);
                        menu.close();
                    }
                },
            );
        });
    });
    cmd
}

/// Color pair for the auth badge: text on chip background.
fn auth_palette(label: &str) -> (Color32, Color32) {
    match label {
        "Key" => (theme::VIOLET, theme::VIOLET_BG),
        _ => (theme::INFO, theme::INFO_BG),
    }
}

fn tunnel_row(
    ui: &mut Ui,
    session_name: &str,
    conn: &str,
    forward: &PortForward,
    running: bool,
    reconnecting: bool,
    auth_label: &str,
) -> Option<RowAction> {
    let mut action = None;
    let (dot_color, dot_tip) = if reconnecting {
        (theme::WARNING, "SSH link down — reconnecting automatically")
    } else if running {
        (theme::SUCCESS, "Running")
    } else {
        (theme::TEXT_FAINT, "Stopped")
    };
    let (a_fg, a_bg) = auth_palette(auth_label);

    Frame::NONE
        .fill(theme::BG_CARD)
        .corner_radius(CornerRadius::same(10))
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .inner_margin(Margin::symmetric(12, 9))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("●").size(10.0).color(dot_color))
                    .on_hover_text(dot_tip);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.label(RichText::new(forward_desc(forward)).size(13.0));
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let (t_fg, t_bg) = fwd_palette(&forward.fwd_type);
                        chip(ui, forward.fwd_type.short(), t_fg, t_bg);
                        chip(ui, auth_label, a_fg, a_bg);
                        if forward.auto_reconnect {
                            chip(ui, "↻ auto", theme::ACCENT_TEXT, theme::ACCENT_SOFT)
                                .on_hover_text("Auto-reconnect enabled");
                        }
                        ui.label(
                            RichText::new(format!("{session_name} · {conn}"))
                                .size(11.0)
                                .color(theme::TEXT_FAINT),
                        );
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (start_fg, start_bg, start_hover) = if running {
                        (theme::DANGER, theme::DANGER_BG, Color32::from_rgb(248, 226, 226))
                    } else {
                        (theme::SUCCESS, theme::SUCCESS_BG, Color32::from_rgb(223, 241, 232))
                    };
                    if soft_pill(ui, if running { "Stop" } else { "Start" }, start_fg, start_bg, start_hover)
                        .clicked()
                    {
                        action = Some(RowAction::Toggle);
                    }
                    if ghost_button(ui, RichText::new("Edit").size(11.5).color(theme::TEXT_DIM), false)
                        .clicked()
                    {
                        action = Some(RowAction::Edit);
                    }
                    if ghost_button(ui, RichText::new("✕").size(12.0).color(theme::TEXT_DIM), true)
                        .on_hover_text("Delete tunnel")
                        .clicked()
                    {
                        action = Some(RowAction::Delete);
                    }
                    let auto = forward.auto_reconnect;
                    if soft_pill(
                        ui,
                        "↻ auto",
                        if auto { theme::ACCENT_TEXT } else { theme::TEXT_FAINT },
                        if auto { theme::ACCENT_SOFT } else { theme::BG_PANEL_ALT },
                        theme::ACCENT_SOFT_HOVER,
                    )
                    .on_hover_text(if auto {
                        "Auto-reconnect: on — click to turn off"
                    } else {
                        "Auto-reconnect: off — reconnects the tunnel automatically if it drops"
                    })
                    .clicked()
                    {
                        action = Some(RowAction::ToggleAuto);
                    }
                });
            });
        });
    ui.add_space(6.0);
    action
}

/// Terminal-style tab chip; right-click opens the Chrome-style tab menu.
/// Returns (clicked-activate, clicked-close, context-menu command).
fn tab_chip(
    ui: &mut Ui,
    title: &str,
    dot: Option<Color32>,
    active: bool,
    closable: bool,
) -> (bool, bool, Option<TabMenu>) {
    let mut clicked = false;
    let mut closed = false;
    let mut menu = None;
    Frame::NONE
        .fill(if active {
            theme::BG_CARD
        } else {
            Color32::TRANSPARENT
        })
        .corner_radius(CornerRadius::same(8))
        .stroke(if active {
            Stroke::new(1.0_f32, theme::BORDER_STRONG)
        } else {
            Stroke::NONE
        })
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            if active {
                // Top accent strip — a quiet callback to browser tabs.
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 2.0),
                    Sense::hover(),
                );
                ui.painter().rect_filled(rect, 1.0, theme::ACCENT);
            }
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if let Some(c) = dot {
                    ui.label(RichText::new("●").size(9.0).color(c));
                }
                let mut label = RichText::new(title).size(13.0);
                if active {
                    label = label.strong();
                }
                let label = ui
                    .add(egui::Label::new(label).sense(Sense::click()))
                    .on_hover_text(format!("{title} — right-click for tab options"));
                if label.clicked() {
                    clicked = true;
                }
                label.context_menu(|ui| {
                    if closable && ui.button("Close tab").clicked() {
                        menu = Some(TabMenu::CloseSelf);
                        ui.close();
                    }
                    if ui.button("Close tabs to the right").clicked() {
                        menu = Some(TabMenu::CloseRight);
                        ui.close();
                    }
                    if ui.button("Close other tabs").clicked() {
                        menu = Some(TabMenu::CloseOthers);
                        ui.close();
                    }
                    if ui.button("Close all tabs").clicked() {
                        menu = Some(TabMenu::CloseAll);
                        ui.close();
                    }
                });
                if closable {
                    let x = ui
                        .add(
                            egui::Button::new(
                                RichText::new("✕").size(10.5).color(theme::TEXT_FAINT),
                            )
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::NONE)
                            .corner_radius(CornerRadius::same(4)),
                        )
                        .on_hover_text("Close tab");
                    if x.clicked() {
                        closed = true;
                    }
                }
            });
        });
    (clicked, closed, menu)
}

fn chip(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) -> egui::Response {
    Frame::NONE
        .fill(bg)
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(10.5).color(fg).strong());
        })
        .response
}

fn choice_pill(ui: &mut Ui, label: &str, active: bool) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(12.5))
            .fill(if active {
                theme::ACCENT_SOFT
            } else {
                theme::BG_BTN
            })
            .stroke(Stroke::new(
                1.0_f32,
                if active {
                    theme::ACCENT
                } else {
                    theme::BORDER
                },
            ))
            .corner_radius(CornerRadius::same(7)),
    )
}

fn field(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.label(RichText::new(label).size(12.0).color(theme::TEXT_DIM));
    add(ui);
    ui.add_space(8.0);
}

/// Solid primary (or danger) button with real hover feedback. egui paints
/// regular buttons from `weak_bg_fill`, so we scope style overrides instead
/// of hard-coding `.fill(...)`, which would kill hover/active states. Text
/// passed without an explicit color follows the widget state fg_stroke.
fn styled_button(
    ui: &mut Ui,
    text: RichText,
    enabled: bool,
    danger: bool,
    size: Option<egui::Vec2>,
) -> egui::Response {
    // Solid buttons read best with white text; leave disabled labels to the
    // caller (usually a muted color for the grayed-out look).
    let text = if enabled { text.color(Color32::WHITE) } else { text };
    ui.scope(|ui| {
        let v = &mut ui.style_mut().visuals;
        let (fill, hover) = if danger {
            (theme::DANGER_BTN, theme::DANGER_BTN_HOVER)
        } else {
            (theme::ACCENT, theme::ACCENT_HOVER)
        };
        for w in ["inactive", "hovered", "active"] {
            let widget = match w {
                "inactive" => &mut v.widgets.inactive,
                "hovered" => &mut v.widgets.hovered,
                _ => &mut v.widgets.active,
            };
            widget.weak_bg_fill = fill;
            widget.bg_fill = fill;
            widget.bg_stroke = Stroke::NONE;
            widget.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
        }
        v.widgets.hovered.weak_bg_fill = hover;
        v.widgets.hovered.bg_fill = hover;
        let button = egui::Button::new(text).corner_radius(CornerRadius::same(8));
        ui.add_enabled_ui(enabled, |ui| match size {
            Some(s) => ui.add_sized(s, button),
            None => ui.add(button),
        })
        .inner
    })
    .inner
}

/// Small rounded pill used for status actions inside rows (Start/Stop,
/// auto-reconnect toggle). `fg` colors the label, `bg`/`hover_bg` the chip.
fn soft_pill(ui: &mut Ui, label: &str, fg: Color32, bg: Color32, hover_bg: Color32) -> egui::Response {
    ui.scope(|ui| {
        let v = &mut ui.style_mut().visuals;
        for (state, fill) in [("inactive", bg), ("hovered", hover_bg), ("active", hover_bg)] {
            let widget = match state {
                "inactive" => &mut v.widgets.inactive,
                "hovered" => &mut v.widgets.hovered,
                _ => &mut v.widgets.active,
            };
            widget.weak_bg_fill = fill;
            widget.bg_fill = fill;
            widget.bg_stroke = Stroke::NONE;
        }
        ui.spacing_mut().button_padding = egui::vec2(9.0, 3.0);
        ui.add(
            egui::Button::new(RichText::new(label).size(11.5).strong().color(fg))
                .corner_radius(CornerRadius::same(7)),
        )
    })
    .inner
}

/// Borderless text button that reveals a soft background on hover.
/// `danger` tints the hover background red (delete-style affordances).
fn ghost_button(ui: &mut Ui, text: RichText, danger: bool) -> egui::Response {
    ui.scope(|ui| {
        let v = &mut ui.style_mut().visuals;
        let hover = if danger {
            theme::DANGER_BG
        } else {
            theme::BG_BTN_HOVER
        };
        for state in ["inactive", "hovered", "active"] {
            let widget = match state {
                "inactive" => &mut v.widgets.inactive,
                "hovered" => &mut v.widgets.hovered,
                _ => &mut v.widgets.active,
            };
            widget.weak_bg_fill = Color32::TRANSPARENT;
            widget.bg_fill = Color32::TRANSPARENT;
            widget.bg_stroke = Stroke::NONE;
        }
        v.widgets.hovered.weak_bg_fill = hover;
        v.widgets.hovered.bg_fill = hover;
        v.widgets.active.weak_bg_fill = hover;
        v.widgets.active.bg_fill = hover;
        ui.spacing_mut().button_padding = egui::vec2(5.0, 2.0);
        ui.add(
            egui::Button::new(text)
                .fill(Color32::TRANSPARENT)
                .corner_radius(CornerRadius::same(6)),
        )
    })
    .inner
}

fn empty_hint(ui: &mut Ui, title: &str, body: &str) {
    Frame::NONE
        .fill(theme::BG_PANEL_ALT)
        .corner_radius(CornerRadius::same(12))
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(title).size(14.0).strong());
                ui.add_space(2.0);
                ui.label(
                    RichText::new(body)
                        .size(12.0)
                        .color(theme::TEXT_DIM),
                );
            });
        });
}

fn paint_mark(ui: &mut Ui, texture: &egui::TextureHandle, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), Sense::hover());
    ui.painter().add(egui::Shape::image(
        texture.id(),
        rect,
        egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    ));
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn forward_desc(f: &PortForward) -> String {
    match f.fwd_type {
        ForwardType::Local => format!(
            "{}:{} → {}:{}",
            f.listen_host, f.listen_port, f.dest_host, f.dest_port
        ),
        ForwardType::Remote => format!(
            "server {}:{} → {}:{}",
            f.listen_host, f.listen_port, f.dest_host, f.dest_port
        ),
        ForwardType::Dynamic => format!("SOCKS5 proxy on {}:{}", f.listen_host, f.listen_port),
    }
}

fn fwd_palette(t: &ForwardType) -> (Color32, Color32) {
    match t {
        ForwardType::Local => (theme::INFO, theme::INFO_BG),
        ForwardType::Remote => (theme::VIOLET, theme::VIOLET_BG),
        ForwardType::Dynamic => (theme::SUCCESS, theme::SUCCESS_BG),
    }
}

fn ssh_command(s: &SshSession) -> String {
    format!(
        "ssh -p {} {}@{}",
        s.port,
        s.username.trim(),
        s.host.trim()
    )
}

fn with_alpha(c: Color32, alpha: f32) -> Color32 {
    let a = (alpha.clamp(0.0, 1.0) * 255.0) as u8;
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn demo_session() -> SshSession {
        let mut s = SshSession::default();
        s.id = "demo-session".to_string();
        s.name = "Demo server".to_string();
        s.host = "10.0.0.5".to_string();
        s.username = "root".to_string();
        s.password = "hunter2".to_string();
        s.forwards.push(PortForward::default());
        s
    }

    fn demo_app() -> (TunnelApp, tokio::runtime::Runtime) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let logs: SharedLogs = Arc::new(Mutex::new(Vec::new()));
        let mut app = TunnelApp::new_inner(rt.handle().clone(), logs);
        app.sessions.push(demo_session());
        app.selected = Some(app.sessions[0].id.clone());
        (app, rt)
    }

    fn run_frames(
        app: &mut TunnelApp,
        ctx: &egui::Context,
        size: egui::Vec2,
        frames: usize,
    ) -> usize {
        let mut shapes = 0;
        let mut time = 0.0;
        for _ in 0..frames {
            time += 0.016;
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                time: Some(time),
                ..Default::default()
            };
            let output = ctx.run(raw, |ctx| app.ui(ctx));
            shapes = output.shapes.len();
        }
        shapes
    }

    #[test]
    fn ui_renders_headless_without_panics() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);

        // Named styles must resolve against the applied style — this is the
        // exact class of bug that crashed the app when the OS light theme
        // swapped in egui's default styles.
        let style = ctx.style();
        assert!(style.text_styles.contains_key(&theme::h1()));
        assert!(style.text_styles.contains_key(&theme::h2()));

        let (mut app, rt) = demo_app();
        assert!(run_frames(&mut app, &ctx, egui::vec2(1300.0, 860.0), 3) > 30);

        // Tunnels page with a tunnel row + confirm modal.
        app.confirm = Some(Confirm::Tunnel {
            key: forward_key("demo-session", &app.sessions[0].forwards[0].id),
            session_id: "demo-session".to_string(),
            forward_id: app.sessions[0].forwards[0].id.clone(),
            label: "test".to_string(),
        });
        assert!(run_frames(&mut app, &ctx, egui::vec2(1300.0, 860.0), 2) > 40);
        app.confirm = None;

        // Tunnel editor modal.
        app.tunnel_editor = Some(TunnelEditor::new("demo-session".to_string()));
        assert!(run_frames(&mut app, &ctx, egui::vec2(1300.0, 860.0), 2) > 50);
        app.tunnel_editor = None;

        // Session editor modal.
        app.session_editor = Some(SessionEditor::new());
        assert!(run_frames(&mut app, &ctx, egui::vec2(1024.0, 700.0), 2) > 50);
        app.session_editor = None;

        // Activity modal.
        app.show_logs = true;
        assert!(run_frames(&mut app, &ctx, egui::vec2(1024.0, 700.0), 2) > 40);
        app.show_logs = false;

        // Toast.
        app.now = 1.0;
        app.toast(ToastKind::Success, "Tunnel started");
        assert!(run_frames(&mut app, &ctx, egui::vec2(1024.0, 700.0), 2) > 40);

        // Terminal tab: spawns against a closed port, fails fast, renders
        // both the connecting and disconnected overlays.
        app.open_terminal(&ctx, "demo-session");
        assert!(matches!(app.tabs.last(), Some(Tab::Terminal(_))));
        let _ = run_frames(&mut app, &ctx, egui::vec2(1024.0, 700.0), 6) > 50;
        let _ = rt;
        assert!(app.tabs.len() >= 2);
    }

    #[test]
    fn forward_descriptions_read_well() {
        let local = PortForward {
            id: "a".into(),
            fwd_type: ForwardType::Local,
            listen_host: "127.0.0.1".into(),
            listen_port: 8080,
            dest_host: "10.0.0.9".into(),
            dest_port: 80,
            auto_reconnect: true,
        };
        assert_eq!(forward_desc(&local), "127.0.0.1:8080 → 10.0.0.9:80");
        let dynf = PortForward {
            fwd_type: ForwardType::Dynamic,
            ..local.clone()
        };
        assert!(forward_desc(&dynf).contains("SOCKS5"));
    }

    #[test]
    fn ssh_command_includes_port_and_user() {
        let s = demo_session();
        let cmd = ssh_command(&s);
        assert!(cmd.contains("root@10.0.0.5"));
        assert!(cmd.contains("-p 22"));
    }
}
