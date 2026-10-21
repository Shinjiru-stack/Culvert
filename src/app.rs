//! Application shell.
//!
//! Layout: a narrow sidebar with saved sessions (double-click opens a shell
//! tab), a tabbed workspace whose first tab is the tunnel manager, and a
//! one-line status bar. Everything is UI-driven — no commands anywhere.

use crate::ai::{AiPanel, UiAction};
use crate::log::{push_log, LogEntry, LogLevel, SharedLogs};
use crate::mcp;
use crate::session::{
    load_groups, load_sessions, load_tunnels, save_groups, save_sessions, save_tunnels, AuthType,
    ForwardType, PortForward, Protocol, SessionGroup, SshSession, StandaloneTunnel,
};
use crate::terminal::{TermStatus, TerminalSession};
use crate::theme;
use crate::theme::tglyph;
use crate::tunnel::{forward_key, standalone_key, TunnelManager, TunnelPhase};
use crate::version;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
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
    /// Tunnels with their own SSH endpoint (no saved session required).
    standalone_tunnels: Vec<StandaloneTunnel>,
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
    /// Split view: show every open shell side-by-side in a grid.
    split_view: bool,
    /// Terminal focus mode: the shell fills the window and the OS window
    /// itself goes full-screen (F11 toggles).
    term_fullscreen: bool,
    /// Last full-screen state pushed to the OS window, so the command is only
    /// sent on transitions (and external exits are noticed).
    term_os_fullscreen: bool,
    /// True once the OS has reported the window as full-screen; guards against
    /// reading a stale `false` on the frame right after the command is sent.
    term_os_confirmed: bool,
    /// Session id of a terminal living in its own OS window ("pop out").
    /// Stored by id, not index, so closing/reordering tabs cannot point at
    /// the wrong shell.
    term_detached: Option<String>,
    /// The popped-out window has already been asked to focus itself. One
    /// request per detach is enough; re-requesting every frame would steal
    /// focus back whenever the user clicks the main window.
    term_detached_focused: bool,
    /// MCP server (Tools > MCP server…); None while stopped.
    mcp: Option<mcp::Server>,
    mcp_cfg: mcp::McpConfig,
    mcp_modal: bool,
    mcp_error: Option<String>,
    /// The saved preference is honoured once per run; a failed bind must not
    /// retry every frame.
    mcp_started_once: bool,
    mcp_reveal: bool,
    now: f64,
    mark_texture: Option<egui::TextureHandle>,
    /// User-configured local editors shared by every files pane.
    editors: crate::filebrowser::SharedEditors,
    /// Sidebar tool: saved-session list or docked server file browser.
    side_mode: SideMode,
    /// MobaXterm-style compact state: sidebar shrinks to a slim icon rail.
    sidebar_collapsed: bool,
    /// The one docked browser (sidebar Files tool), tied to the focused
    /// connected session: (session id, browser).
    docked: Option<(String, crate::filebrowser::FileBrowser)>,
    /// One-shot CLI startup action: (session name, open file browser).
    startup: Option<(String, bool)>,
    /// Saved command snippets (macros) for the terminal quick-bar.
    snippets: Vec<crate::snippets::Snippet>,
    /// Snippet manager modal open, with the pre-edit backup for Cancel.
    snippets_open: bool,
    snippets_backup: Vec<crate::snippets::Snippet>,
    /// Split view column ratio (draggable divider), 0.25–0.75.
    split_ratio: f32,
    /// Quick-connect target input: user@host:port.
    quick_connect: String,
    /// AI Copilot: right-hand chat panel + engine channels + settings.
    ai: AiPanel,
    /// A Copilot command currently being typed into a shell tab (interactive
    /// runs answer the engine when the completion marker shows up).
    ai_term: Option<AiTermRun>,
}

/// One Copilot `RunInTerminal` action in flight: the app opens/finds the
/// session's shell tab, types the command plus start/exit markers, and reads
/// the screen back once the shell prints the exit marker.
struct AiTermRun {
    id: u64,
    session_id: String,
    label: String,
    command: String,
    nonce: String,
    /// The tab was requested (opening it happens once).
    opened: bool,
    /// The command has been typed into the shell.
    sent: bool,
    /// Interactive programs are handed over to the user: the run completes as
    /// soon as the command is typed, instead of waiting for an exit marker.
    wait_for_exit: bool,
    /// How many times `sudo`'s password prompt has been answered for this run.
    sudo_fed: u8,
    /// When the prompt was first seen. `sudo` can still have terminal echo on
    /// for the instant between printing the prompt and turning it off, so the
    /// password is typed a beat later instead of the moment the prompt shows.
    sudo_prompt_at: Option<std::time::Instant>,
    armed_at: std::time::Instant,
    sent_at: std::time::Instant,
}

impl AiTermRun {
    fn start_tag(&self) -> String {
        format!("<<<culvert-{}-start>>>", self.nonce)
    }
    fn exit_tag(&self) -> String {
        format!("<<<culvert-{}-exit:", self.nonce)
    }
    /// Printed after a failed command together with the `command -v` probe.
    fn pm_tag(&self) -> String {
        format!("<<<culvert-{}-pm>>>", self.nonce)
    }

    /// The single line typed into the shell. The command itself goes through
    /// `sh -c '<single-quoted>'` so multi-line scripts and quotes survive, and
    /// the markers bracket its output on the screen. The printed markers are
    /// built with `%s` placeholders so the *echo of the typed line* does not
    /// contain them (which would let the reader match too early). On failure
    /// the shell also probes which package managers exist, so the result can
    /// carry a `[hint]` naming the host's real manager (apt vs dnf vs apk…).
    fn line(&self) -> String {
        format!(
            "printf '\\n<<<culvert-%s-start>>>\\n' {n}; sh -c '{cmd}'; rc=$?; if [ \"$rc\" -ne 0 ]; then printf '\\n<<<culvert-%s-pm>>>\\n' {n}; for c in apt-get dnf yum apk zypper pacman; do command -v \"$c\" 2>/dev/null; done; fi; printf '\\n<<<culvert-%s-exit:%s>>>\\n' {n} \"$rc\"\n",
            n = self.nonce,
            cmd = self.command.replace('\'', "'\\''"),
        )
    }
}

/// Remove a secret from text captured off the screen.
fn scrub_secret(text: String, secret: Option<&str>) -> String {
    match secret {
        Some(s) if !s.is_empty() => text.replace(s, "[REDACTED]"),
        _ => text,
    }
}

/// Is the shell waiting at `sudo`'s password prompt? That prompt is always the
/// last thing on screen — nothing else prints until it is answered.
fn at_sudo_prompt(screen: &str) -> bool {
    let last = screen
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    last.starts_with("[sudo] password for") && last.ends_with(':')
}

/// Pull `(exit status, output)` out of the screen once the exit marker is in
/// it. `None` while the marker has not appeared yet.
fn ai_term_result(screen: &str, run: &AiTermRun) -> Option<String> {
    let exit_tag = run.exit_tag();
    let done_at = screen.find(&exit_tag)?;
    let code = screen[done_at + exit_tag.len()..]
        .split(">>>")
        .next()
        .unwrap_or("?")
        .trim()
        .to_string();
    let start_tag = run.start_tag();
    let body = match screen.find(&start_tag) {
        Some(s) => &screen[s + start_tag.len()..done_at],
        // The start scrolled off: hand over the visible tail.
        None => &screen[..done_at],
    };
    // A failed command is followed by the shell's `command -v` probe (see
    // `AiTermRun::line`); split it off so it does not pollute the output.
    let pm_tag = run.pm_tag();
    let (body, probe) = match body.rfind(&pm_tag) {
        Some(at) => (&body[..at], &body[at + pm_tag.len()..]),
        None => (body, ""),
    };
    let body = body
        .lines()
        // The prompt is answered for the user; the model does not need it (and
        // it must never carry anything typed into it).
        .filter(|l| !l.trim_start().starts_with("[sudo] password for"))
        .map(|l| l.trim_end())
        .collect::<Vec<_>>()
        .join("\n");
    let body = body.trim();
    let mut body = if body.is_empty() {
        "(no output)".to_string()
    } else {
        crate::ai::tools::diagnostic_hint(&crate::ai::tools::needs_root_hint(
            body,
            &run.command,
        ))
    };
    if let Some(hint) = crate::ai::tools::terminal_pm_hint(&body, probe) {
        body.push('\n');
        body.push_str(&hint);
    }
    Some(format!("[exit {code}]\n{body}"))
}

enum Tab {
    Tunnels,
    /// Session window: shell + SFTP files pane behind a Shell | Files switcher.
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
    protocol: Protocol,
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
            protocol: Protocol::Ssh,
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
            protocol: s.protocol.clone(),
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

/// Sentinel session id for tunnels that carry their own SSH connection
/// (no saved session).
const STANDALONE_TUNNEL: &str = "__standalone__";

struct TunnelEditor {
    /// True: tunnel stores its own SSH endpoint (nothing else required).
    standalone: bool,
    /// For standalone edits: the tunnel's id.
    tunnel_id: Option<String>,
    /// Standalone display label (optional).
    name: String,
    host: String,
    ssh_port: String,
    username: String,
    auth_type: AuthType,
    password: String,
    key_path: String,
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
            standalone: false,
            tunnel_id: None,
            name: String::new(),
            host: String::new(),
            ssh_port: "22".to_string(),
            username: String::new(),
            auth_type: AuthType::Password,
            password: String::new(),
            key_path: String::new(),
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

    /// A tunnel that works on its own: fresh SSH details, no saved session.
    fn new_standalone() -> Self {
        // The sentinel (not an empty id) is what `save_tunnel_editor` keys
        // off, so "New tunnel" saves even with zero saved sessions.
        let mut ed = Self::new(STANDALONE_TUNNEL.to_string());
        ed.standalone = true;
        ed
    }

    fn from(session_id: &str, f: &PortForward) -> Self {
        Self::new_dirless(session_id, Some(f), false)
    }

    fn from_standalone(t: &StandaloneTunnel) -> Self {
        let mut ed = Self::new_dirless(STANDALONE_TUNNEL, t.forwards.first(), true);
        ed.tunnel_id = Some(t.id.clone());
        ed.name = t.name.clone();
        ed.host = t.host.clone();
        ed.ssh_port = t.port.to_string();
        ed.username = t.username.clone();
        ed.auth_type = t.auth_type.clone();
        ed.password = t.password.clone();
        ed.key_path = t.key_path.clone();
        ed
    }

    fn new_dirless(session_id: &str, f: Option<&PortForward>, standalone: bool) -> Self {
        let mut ed = Self::new(session_id.to_string());
        ed.standalone = standalone;
        if let Some(f) = f {
            ed.forward_id = Some(f.id.clone());
            ed.fwd_type = f.fwd_type.clone();
            ed.listen_host = f.listen_host.clone();
            ed.listen_port = f.listen_port.to_string();
            ed.dest_host = f.dest_host.clone();
            ed.dest_port = f.dest_port.to_string();
            ed.auto_reconnect = f.auto_reconnect;
        }
        ed
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
    /// Default connect: session window landing on the files browser.
    Connect,
    /// Explicit shell request from the context menu.
    OpenShell,
    /// SFTP files pane of the session window (SSH only).
    BrowseFiles,
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
    /// Open a pre-filled "new session in this group" editor.
    AddSession,
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

/// Left-sidebar tools, MobaXterm-style: the compact bar shows saved sessions
/// by default and can flip to a docked file browser for the focused server
/// (clicking a session card focuses it; an open session tab wins).
#[derive(Clone, Copy, PartialEq)]
enum SideMode {
    Sessions,
    Files,
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl TunnelApp {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        rt: tokio::runtime::Handle,
        logs: SharedLogs,
        startup: Option<(String, bool)>,
    ) -> Self {
        Self::new_inner(rt, logs, startup)
    }

    fn new_inner(rt: tokio::runtime::Handle, logs: SharedLogs, startup: Option<(String, bool)>) -> Self {
        let sessions = load_sessions();
        let groups = load_groups();
        let standalone_tunnels = load_tunnels();
        let selected = sessions.first().map(|s| s.id.clone());
        let ai = AiPanel::new(rt.clone(), logs.clone());
        let mcp_cfg = mcp::McpConfig::load();
        Self {
            sessions,
            groups,
            standalone_tunnels,
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
            split_view: false,
            term_fullscreen: false,
            term_os_fullscreen: false,
            term_os_confirmed: false,
            term_detached: None,
            term_detached_focused: false,
            mcp: None,
            mcp_cfg,
            mcp_modal: false,
            mcp_error: None,
            mcp_started_once: false,
            mcp_reveal: false,
            group_editor: None,
            now: 0.0,
            mark_texture: None,
            editors: crate::filebrowser::load_editors(),
            side_mode: SideMode::Sessions,
            sidebar_collapsed: false,
            docked: None,
            startup,
            snippets: crate::snippets::load_snippets(),
            snippets_open: false,
            snippets_backup: Vec::new(),
            split_ratio: 0.5,
            quick_connect: String::new(),
            ai,
            ai_term: None,
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

    /// Which session the docked sidebar browser should show: ONLY a session
    /// the user actually connected (an open session tab, focused one wins) —
    /// sidebar selection alone never spawns a browser.
    fn focused_file_session_id(&self) -> Option<String> {
        match self.tabs.get(self.active) {
            Some(Tab::Terminal(t)) => Some(t.session_id.clone()),
            _ => None,
        }
    }

    /// Close the docked browser, if one is pinned to the sidebar.
    fn close_all_docked(&mut self) {
        if let Some((_, fb)) = self.docked.take() {
            fb.close();
        }
    }

    // -----------------------------------------------------------------------
    // AI Copilot glue
    // -----------------------------------------------------------------------

    /// Build the context snapshot the Copilot engine sees each frame. Prompt
    /// text is built only from the summary fields — `profiles` carries
    /// credentials for the executors and never reaches a model.
    fn ai_snapshot(&self) -> crate::ai::UiContext {
        let dock = self.docked.as_ref().map(|(sid, fb)| crate::ai::CtxDock {
            session_id: sid.clone(),
            cwd: fb.cwd().to_string(),
            selected: fb.selected_file().map(|s| s.to_string()),
        });
        let open_session_ids: Vec<String> = self
            .tabs
            .iter()
            .filter_map(|t| match t {
                Tab::Terminal(term) => Some(term.session_id.clone()),
                Tab::Tunnels => None,
            })
            .collect();
        let profiles_summary = self
            .sessions
            .iter()
            .map(|s| crate::ai::CtxSession {
                id: s.id.clone(),
                name: session_display_name(s),
                host: s.host.clone(),
                username: s.username.clone(),
                protocol: match s.protocol {
                    Protocol::Ssh => "ssh".to_string(),
                    Protocol::Rdp => "rdp".to_string(),
                },
                group: s.group.clone(),
            })
            .collect();
        let mut tunnels = Vec::new();
        for s in &self.sessions {
            for f in &s.forwards {
                let key = forward_key(&s.id, &f.id);
                tunnels.push(crate::ai::CtxTunnel {
                    key: key.clone(),
                    session_label: session_display_name(s),
                    summary: forward_desc(f),
                    running: self.tunnels.is_running(&key),
                    auto_reconnect: f.auto_reconnect,
                });
            }
        }
        for t in &self.standalone_tunnels {
            for f in &t.forwards {
                let key = standalone_key(&t.id, &f.id);
                tunnels.push(crate::ai::CtxTunnel {
                    key: key.clone(),
                    session_label: t.label(),
                    summary: forward_desc(f),
                    running: self.tunnels.is_running(&key),
                    auto_reconnect: f.auto_reconnect,
                });
            }
        }
        crate::ai::UiContext {
            active_session_id: self.focused_file_session_id(),
            open_session_ids,
            dock,
            profiles_summary,
            tunnels,
            profiles: self.sessions.clone(),
        }
    }

    /// Execute a Copilot UI action with the app's own code paths and answer
    /// the engine with a human-readable result. `None` means the work runs
    /// asynchronously — see [`Self::tick_ai_terminal`], which answers once the
    /// command typed into the shell tab has finished.
    fn ai_apply_action(
        &mut self,
        ctx: &egui::Context,
        id: u64,
        action: &UiAction,
    ) -> Option<String> {
        if let UiAction::RunInTerminal {
            session_id,
            label,
            command,
        } = action
        {
            let nonce = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
            self.ai_term = Some(AiTermRun {
                id,
                session_id: session_id.clone(),
                label: label.clone(),
                command: command.clone(),
                nonce,
                opened: false,
                sent: false,
                wait_for_exit: crate::ai::tools::interactive_program(command).is_none(),
                sudo_fed: 0,
                sudo_prompt_at: None,
                armed_at: std::time::Instant::now(),
                sent_at: std::time::Instant::now(),
            });
            return None;
        }
        Some(self.ai_apply_action_now(ctx, action))
    }

    /// Drive a Copilot command running in a shell tab: open the tab, type the
    /// command once the shell is up, then hand the screen's output back to the
    /// engine when the exit marker appears.
    fn tick_ai_terminal(&mut self, ctx: &egui::Context) {
        let Some(mut run) = self.ai_term.take() else {
            return;
        };
        // A `sudo` prompt blocks a command typed into the tab; the saved login
        // password answers it (through the PTY, so it is never echoed, never on
        // the command line, never in the audit log and never sent to the model).
        let sudo_pw = self
            .sessions
            .iter()
            .find(|s| s.id == run.session_id)
            .and_then(crate::ai::tools::sudo_password);
        // A cancelled turn no longer wants the answer: stop waiting. The
        // command, if it was already typed, keeps running in its tab.
        if !self.ai.busy {
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(250));

        let Some(idx) = self.find_tab_for_session(&run.session_id) else {
            if !run.opened {
                run.opened = true;
                self.open_session(ctx, &run.session_id);
            } else if run.armed_at.elapsed() > Duration::from_secs(60) {
                self.ai.complete_action(
                    run.id,
                    format!("error: could not open a shell to {} (check the profile)", run.label),
                );
                return;
            }
            self.ai_term = Some(run);
            return;
        };

        let Some(Tab::Terminal(term)) = self.tabs.get_mut(idx) else {
            self.ai_term = Some(run);
            return;
        };
        if !run.sent {
            if !matches!(term.status, TermStatus::Connected) {
                if run.armed_at.elapsed() > Duration::from_secs(60) {
                    self.ai.complete_action(
                        run.id,
                        format!("error: the shell to {} did not connect in time", run.label),
                    );
                    return;
                }
                self.ai_term = Some(run);
                return;
            }
            term.send_bytes(run.line().into_bytes());
            run.sent = true;
            run.sent_at = std::time::Instant::now();
            // Show the tab so the user can watch (and answer prompts).
            self.active = idx;
            ctx.request_repaint();
            if !run.wait_for_exit {
                // Full-screen program: hand the terminal over and let the
                // model move on instead of blocking on an exit marker.
                self.ai.complete_action(
                    run.id,
                    format!(
                        "[started] `{}` is running in the terminal tab now — it is a full-screen program, so Culvert handed the terminal to the user instead of capturing its output. It keeps running until the user quits it; you can report that it started.",
                        run.command
                    ),
                );
                return;
            }
            self.ai_term = Some(run);
            return;
        }
        // Answer `sudo`'s password prompt while the command waits for it. The
        // prompt is the last line on screen and nothing else prints until it is
        // answered, so matching it is unambiguous; two attempts let a rejected
        // password fail visibly instead of looping.
        if run.sudo_fed < 2 {
            if let Some(pw) = sudo_pw.as_deref() {
                if at_sudo_prompt(&term.visible_text()) {
                    match run.sudo_prompt_at {
                        // First sighting: wait for `sudo` to switch echo off.
                        None => run.sudo_prompt_at = Some(std::time::Instant::now()),
                        Some(seen)
                            if seen.elapsed() >= Duration::from_millis(400) =>
                        {
                            term.send_bytes(format!("{pw}\n").into_bytes());
                            run.sudo_fed += 1;
                            run.sudo_prompt_at = None;
                            ctx.request_repaint();
                        }
                        _ => {}
                    }
                } else {
                    run.sudo_prompt_at = None;
                }
            }
        }
        if let Some(result) = ai_term_result(&term.visible_text(), &run) {
            // Belt and braces: if the password ever reached the screen it must
            // not travel on to the model or the audit log.
            self.ai.complete_action(run.id, scrub_secret(result, sudo_pw.as_deref()));
            ctx.request_repaint();
            return;
        }
        if run.sent_at.elapsed() > Duration::from_secs(300) {
            self.ai.complete_action(
                run.id,
                format!(
                    "error: no result after 5 minutes — it may still be running in the terminal tab ({}) or waiting for input there",
                    run.label
                ),
            );
            return;
        }
        self.ai_term = Some(run);
    }

    fn ai_apply_action_now(&mut self, ctx: &egui::Context, action: &UiAction) -> String {
        match action {
            UiAction::OpenSession { session_id } => {
                let name = self
                    .sessions
                    .iter()
                    .find(|s| &s.id == session_id)
                    .map(session_display_name)
                    .unwrap_or_else(|| session_id.clone());
                self.open_session(ctx, session_id);
                format!("Opened the session '{name}'.")
            }
            UiAction::CloseSession { session_id } => match self.find_tab_for_session(session_id) {
                Some(i) => {
                    self.close_tab(i);
                    "Closed the session window.".to_string()
                }
                None => "That session has no open window right now.".to_string(),
            },
            UiAction::StartTunnel { key } => {
                if self.tunnels.is_running(key) {
                    return "That tunnel is already running.".to_string();
                }
                let Some((session, forward)) = self.locate_tunnel(key) else {
                    return "That tunnel no longer exists.".to_string();
                };
                match self.tunnels.start(&session, &forward) {
                    Ok(()) => {
                        push_log(
                            &self.logs,
                            LogEntry::new(
                                LogLevel::Info,
                                "AI",
                                &format!("tunnel started · {}", forward_desc(&forward)),
                            ),
                        );
                        format!("Tunnel started: {}.", forward_desc(&forward))
                    }
                    Err(e) => format!("Could not start the tunnel: {e}"),
                }
            }
            UiAction::StopTunnel { key } => {
                if !self.tunnels.is_running(key) {
                    return "That tunnel is not running.".to_string();
                }
                self.tunnels.stop(key);
                "Tunnel stopped.".to_string()
            }
            UiAction::DockNavigate { session_id, path } => {
                self.sidebar_collapsed = false;
                self.side_mode = SideMode::Files;
                match &self.docked {
                    Some((sid, fb)) if sid == session_id => {
                        fb.navigate(path);
                        format!("File browser navigated to {path}.")
                    }
                    _ => {
                        "The file browser is not showing that session right now — open its shell window first, then ask again."
                            .to_string()
                    }
                }
            }
            UiAction::OpenFile { session_id, path } => {
                self.sidebar_collapsed = false;
                self.side_mode = SideMode::Files;
                match &mut self.docked {
                    Some((sid, fb)) if sid == session_id => {
                        fb.open_path_in_editor(path);
                        format!(
                            "Opened {path} in the local editor; saves re-upload automatically."
                        )
                    }
                    _ => {
                        "The file browser is not showing that session right now — open its shell window first, then ask again."
                            .to_string()
                    }
                }
            }
            UiAction::RunInTerminal { .. } => {
                "Internal: terminal runs are answered asynchronously.".to_string()
            }
        }
    }

    /// Resolve a forward key back to the owning session + forward (saved
    /// sessions first, then standalone tunnels).
    fn locate_tunnel(&self, key: &str) -> Option<(SshSession, PortForward)> {
        for s in &self.sessions {
            for f in &s.forwards {
                if forward_key(&s.id, &f.id) == key {
                    return Some((s.clone(), f.clone()));
                }
            }
        }
        for t in &self.standalone_tunnels {
            for f in &t.forwards {
                if standalone_key(&t.id, &f.id) == key {
                    return Some((t.to_ssh_session(), f.clone()));
                }
            }
        }
        None
    }

    /// Swap the docked browser when the focused (connected) session changes:
    /// one live SFTP connection, tied to whoever owns the foreground tab.
    fn sync_dock_focus(&mut self, ctx: &egui::Context, focused: &str) {
        let mismatch = match &self.docked {
            Some((id, _)) => id != focused,
            None => true,
        };
        if mismatch {
            self.close_all_docked();
            if let Some(session) = self
                .sessions
                .iter()
                .find(|s| s.id == focused)
                .cloned()
            {
                let fb = crate::filebrowser::FileBrowser::spawn(
                    ctx,
                    self.rt.clone(),
                    self.logs.clone(),
                    self.editors.clone(),
                    session,
                );
                self.docked = Some((focused.to_string(), fb));
            }
        }
    }

    /// mstsc refuses to dial loopback targets altogether and fails with the
    /// misleading error "you already have a console session in progress" —
    /// even when a WSL/docker relay is listening on 127.0.0.1. When the
    /// session host is loopback, dial the WSL VM address instead
    /// (`wsl hostname -I`); the relay serves that address too.
    fn rdp_target_host(host: &str) -> String {
        let trimmed = host.trim().to_string();
        let loopback = trimmed.eq_ignore_ascii_case("localhost")
            || trimmed == "127.0.0.1"
            || trimmed == "[::1]"
            || trimmed == "::1";
        if !loopback {
            return trimmed;
        }
        let Ok(out) = std::process::Command::new("wsl")
            .args(["hostname", "-I"])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .output()
        else {
            return trimmed;
        };
        match String::from_utf8_lossy(&out.stdout).split_whitespace().next() {
            Some(ip) if !ip.is_empty() => ip.to_string(),
            _ => trimmed,
        }
    }

    /// Launch the Windows RDP client for a session — MobaXterm-style: when
    /// the session has a saved username+password they are seeded into the
    /// Windows Credential Manager (`cmdkey /generic:TERMSRV/…`) so the
    /// client's NLA prompt pre-fills and the click-through just works.
    /// NOTE: the password is stored locally in the user's credential store,
    /// the same tradeoff MobaXterm makes for saved RDP logins.
    fn launch_rdp(&mut self, session: &SshSession) {
        let host = Self::rdp_target_host(session.host.trim());
        if host.is_empty() {
            self.toast(ToastKind::Info, "Save this session with a host first");
            return;
        }
        if !session.username.trim().is_empty() && !session.password.is_empty() {
            let generic = format!("TERMSRV/{host}");
            match std::process::Command::new("cmdkey")
                .arg(format!("/generic:{generic}"))
                .arg(format!("/user:{}", session.username.trim()))
                .arg(format!("/pass:{}", session.password))
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .output()
            {
                Ok(_) => {
                    push_log(
                        &self.logs,
                        LogEntry::new(
                            LogLevel::Success,
                            &session.name,
                            &format!("CredentialManager seeded for {generic}"),
                        ),
                    );
                }
                Err(e) => {
                    push_log(
                        &self.logs,
                        LogEntry::new(
                            LogLevel::Error,
                            &session.name,
                            &format!("cmdkey failed: {e}"),
                        ),
                    );
                }
            }
        }
        let target = format!("/v:{}:{}", host, session.port);
        match std::process::Command::new("mstsc").arg(&target).spawn() {
            Ok(_) => {
                push_log(
                    &self.logs,
                    LogEntry::new(
                        LogLevel::Success,
                        &session.name,
                        &format!("Launched RDP client for {target}"),
                    ),
                );
                self.toast(ToastKind::Success, format!("RDP window opened · {}", session.name));
            }
            Err(e) => self.toast(
                ToastKind::Error,
                format!("Could not start mstsc: {e}"),
            ),
        }
    }

    /// One session window per server: a shell pane whose files live in the
    /// sidebar's docked Files browser (which follows the focused session).
    fn open_session(&mut self, ctx: &egui::Context, session_id: &str) {
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
        if session.protocol == Protocol::Rdp {
            // Windows desktop session: hand it to the system RDP client.
            self.launch_rdp(&session);
            return;
        }
        if session.host.trim().is_empty() {
            self.toast(ToastKind::Info, "Save this session with a host first");
            return;
        }
        if let Err(e) = session.validate() {
            self.toast(ToastKind::Error, format!("Cannot open session: {e}"));
            return;
        }
        let term = TerminalSession::spawn(ctx, self.rt.clone(), self.logs.clone(), session);
        self.tabs.push(Tab::Terminal(Box::new(term)));
        self.active = self.tabs.len() - 1;
    }

    /// MobaXterm-style quick connect: `user@host[:port]` → connect now when
    /// the profile has credentials, otherwise prefill the session editor.
    fn quick_connect_go(&mut self, ctx: &egui::Context) {
        use crate::snippets::parse_host_target;
        let target = self.quick_connect.trim().to_string();
        if target.is_empty() {
            self.toast(ToastKind::Info, "Type user@host:port first");
            return;
        }
        match parse_host_target(&target) {
            Err(e) => self.toast(ToastKind::Error, e),
            Ok((user, host, port)) => {
                if port == 3389 {
                    // Windows desktop target: hand it to the system RDP client
                    // with saved-credential seeding.
                    let mut session = SshSession::default();
                    session.name = format!("{user}@{host}");
                    session.protocol = Protocol::Rdp;
                    session.host = host;
                    session.port = port;
                    session.username = user;
                    self.launch_rdp(&session);
                    return;
                }
                let mut session = SshSession::default();
                session.name = format!("{user}@{host}");
                session.host = host;
                session.port = port;
                session.username = user;
                if let Err(e) = session.validate() {
                    self.toast(ToastKind::Error, format!("Cannot connect: {e}"));
                    return;
                }
                if session.auth_type == AuthType::Password && session.password.is_empty() {
                    // No credentials on file — prefill the session editor.
                    let mut ed = SessionEditor::new();
                    ed.name = session.name.clone();
                    ed.host = session.host.clone();
                    ed.port = port.to_string();
                    ed.username = session.username.clone();
                    self.session_editor = Some(ed);
                    self.toast(
                        ToastKind::Info,
                        "Enter credentials and save — then double-click the session to connect",
                    );
                } else {
                    let term = TerminalSession::spawn(ctx, self.rt.clone(), self.logs.clone(), session);
                    self.tabs.push(Tab::Terminal(Box::new(term)));
                    self.active = self.tabs.len() - 1;
                }
            }
        }
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

    /// Move the active tab by `step`, wrapping at both ends. Used by
    /// Ctrl+Tab / Ctrl+Shift+Tab.
    fn cycle_tab(&mut self, step: isize) {
        if self.tabs.len() < 2 {
            return;
        }
        let len = self.tabs.len() as isize;
        self.active = (self.active as isize + step).rem_euclid(len) as usize;
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
        if let Some(f) = self
            .sessions
            .iter_mut()
            .find(|s| s.id == session_id)
            .and_then(|s| s.forwards.iter_mut().find(|f| f.id == forward_id))
        {
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
            return;
        }
        // Standalone tunnel (own SSH endpoint, lives in tunnels.json).
        if let Some(f) = self
            .standalone_tunnels
            .iter_mut()
            .find(|t| t.id == session_id)
            .and_then(|t| t.forwards.iter_mut().find(|f| f.id == forward_id))
        {
            f.auto_reconnect = !f.auto_reconnect;
            let (on, desc) = (f.auto_reconnect, forward_desc(f));
            let key = standalone_key(session_id, forward_id);
            if self.tunnels.is_running(&key) {
                self.tunnels.set_auto_reconnect(&key, on);
                self.toast(
                    ToastKind::Info,
                    format!("{desc} — auto-reconnect {} for the running tunnel", if on { "enabled" } else { "disabled" }),
                );
            }
            self.persist_tunnels(if on { "Auto-reconnect enabled" } else { "Auto-reconnect disabled" });
        }
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
        // Resolve first: a standalone tunnel runs under a synthetic session
        // with a `tunnel-` id prefix, so the running key comes from the
        // resolved session, never from the row's id.
        // Session-bound tunnel first; standalone tunnels (own SSH endpoint)
        // fall back to an engine-shaped session built on the fly.
        let found = self
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .and_then(|s| {
                s.forwards
                    .iter()
                    .find(|f| f.id == forward_id)
                    .map(|f| (s.clone(), f.clone()))
            })
            .or_else(|| {
                self.standalone_tunnels
                    .iter()
                    .find(|t| t.id == session_id)
                    .and_then(|t| {
                        t.forwards
                            .iter()
                            .find(|f| f.id == forward_id)
                            .map(|f| (t.to_ssh_session(), f.clone()))
                    })
            });
        let Some((session, forward)) = found else { return };
        let key = forward_key(&session.id, &forward.id);
        if self.tunnels.is_running(&key) {
            self.tunnels.stop(&key);
            self.toast(ToastKind::Info, "Tunnel stopped");
            return;
        }
        match self.tunnels.start(&session, &forward) {
            // `start` only binds the local listener / spawns the worker; SSH
            // may still be connecting. The row now shows Connecting and then
            // Running (or Failed with the reason), so don't claim success yet.
            Ok(()) => self.toast(
                ToastKind::Info,
                format!("Connecting tunnel · {}", forward_desc(&forward)),
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

    fn persist_tunnels(&mut self, ok_msg: impl Into<String>) {
        match save_tunnels(&self.standalone_tunnels) {
            Ok(_) => self.toast(ToastKind::Success, ok_msg),
            Err(e) => self.toast(ToastKind::Error, format!("Could not save tunnels: {e}")),
        }
    }

    // -----------------------------------------------------------------------
    // MCP: external agents manage saved sessions and tunnels
    // -----------------------------------------------------------------------

    /// Start the server on the first frame if the user enabled it, then drain
    /// every pending call. Applying on the UI thread keeps one source of truth
    /// (and one save path) for both windows and agents.
    fn tick_mcp(&mut self, ctx: &egui::Context) {
        if self.mcp.is_none() && !self.mcp_started_once && self.mcp_cfg.enabled {
            self.mcp_started_once = true;
            match mcp::Server::start(&self.mcp_cfg, ctx.clone()) {
                Ok(s) => {
                    self.mcp = Some(s);
                    self.mcp_error = None;
                }
                Err(e) => self.mcp_error = Some(e),
            }
        }
        let mut pending = Vec::new();
        if let Some(server) = &self.mcp {
            while let Some(p) = server.try_recv() {
                pending.push(p);
            }
        }
        for p in pending {
            let res = self.apply_mcp(&p.action, ctx);
            // A tool error is normal (bad name, validation); keep it for the
            // modal but do not spam the activity log for every failed lookup.
            if let Err(e) = &res {
                self.mcp_error = Some(e.clone());
            }
            let _ = p.reply.send(res);
        }
    }

    fn apply_mcp(
        &mut self,
        action: &mcp::Action,
        ctx: &egui::Context,
    ) -> Result<serde_json::Value, String> {
        use serde_json::json;
        match action {
            mcp::Action::ListSessions => {
                let list: Vec<_> = self.sessions.iter().map(|s| self.session_json(s)).collect();
                Ok(json!({ "count": list.len(), "sessions": list }))
            }
            mcp::Action::GetSession { name } => {
                let idx = self.find_session_by(name).ok_or_else(|| {
                    format!("no session named '{name}' — call culvert_list_sessions first")
                })?;
                Ok(json!({ "session": self.session_json(&self.sessions[idx]) }))
            }
            mcp::Action::CreateSession { spec } => {
                let user = spec.username.clone().unwrap_or_default().trim().to_string();
                let host = spec.host.clone().unwrap_or_default().trim().to_string();
                let mut s = SshSession::default();
                s.name = spec.name.clone().unwrap_or_default().trim().to_string();
                s.host = host.clone();
                s.port = spec.port.unwrap_or(22);
                s.username = user.clone();
                s.protocol = if spec.protocol.as_deref() == Some("rdp") {
                    Protocol::Rdp
                } else {
                    Protocol::Ssh
                };
                s.auth_type = if spec.auth.as_deref() == Some("key") {
                    AuthType::Key
                } else {
                    AuthType::Password
                };
                s.password = spec.password.clone().unwrap_or_default();
                s.key_path = spec.key_path.clone().unwrap_or_default().trim().to_string();
                s.key_passphrase = spec.key_passphrase.clone().unwrap_or_default();
                if let Some(g) = spec.group.as_deref() {
                    if !g.trim().is_empty() {
                        s.group = Some(self.resolve_group(g)?);
                    }
                }
                if s.name.is_empty() {
                    s.name = format!("{user}@{host}");
                }
                s.validate()?;
                if self
                    .sessions
                    .iter()
                    .any(|x| x.name.eq_ignore_ascii_case(&s.name))
                {
                    return Err(format!("a session named '{}' already exists", s.name));
                }
                let id = s.id.clone();
                let name = s.name.clone();
                self.sessions.push(s);
                self.selected = Some(id.clone());
                save_sessions(&self.sessions)
                    .map_err(|e| format!("kept in memory, but sessions.json failed: {e}"))?;
                self.toast(ToastKind::Success, format!("MCP: session '{name}' created"));
                Ok(json!({ "created": { "id": id, "name": name } }))
            }
            mcp::Action::UpdateSession { name, spec } => {
                let idx = self.find_session_by(name).ok_or_else(|| {
                    format!("no session named '{name}' — call culvert_list_sessions first")
                })?;
                let mut s = self.sessions[idx].clone();
                let was = s.name.clone();
                if let Some(v) = &spec.name {
                    if !v.trim().is_empty() {
                        s.name = v.trim().to_string();
                    }
                }
                if let Some(v) = &spec.host {
                    s.host = v.trim().to_string();
                }
                if let Some(v) = spec.port {
                    s.port = v;
                }
                if let Some(v) = &spec.username {
                    s.username = v.trim().to_string();
                }
                if let Some(v) = &spec.protocol {
                    s.protocol = if v == "rdp" {
                        Protocol::Rdp
                    } else {
                        Protocol::Ssh
                    };
                }
                if let Some(v) = &spec.auth {
                    s.auth_type = if v == "key" {
                        AuthType::Key
                    } else {
                        AuthType::Password
                    };
                }
                // Passwords are write-only: an omitted or empty value keeps
                // the saved one.
                if let Some(v) = &spec.password {
                    if !v.is_empty() {
                        s.password = v.clone();
                    }
                }
                if let Some(v) = &spec.key_path {
                    s.key_path = v.trim().to_string();
                }
                if let Some(v) = &spec.key_passphrase {
                    s.key_passphrase = v.clone();
                }
                if let Some(g) = &spec.group {
                    s.group = if g.trim().is_empty() {
                        None
                    } else {
                        Some(self.resolve_group(g)?)
                    };
                }
                s.validate()?;
                if self
                    .sessions
                    .iter()
                    .enumerate()
                    .any(|(i, x)| i != idx && x.name.eq_ignore_ascii_case(&s.name))
                {
                    return Err(format!("a session named '{}' already exists", s.name));
                }
                let now = s.name.clone();
                self.sessions[idx] = s;
                save_sessions(&self.sessions)
                    .map_err(|e| format!("kept in memory, but sessions.json failed: {e}"))?;
                self.toast(ToastKind::Info, format!("MCP: session '{now}' updated"));
                Ok(json!({ "updated": { "name": now, "was": was } }))
            }
            mcp::Action::DeleteSession { name } => {
                let idx = self.find_session_by(name).ok_or_else(|| {
                    format!("no session named '{name}' — call culvert_list_sessions first")
                })?;
                let removed = self.sessions.remove(idx);
                self.tunnels.stop_for_session(&removed.id);
                self.tabs
                    .retain(|tab| !matches!(tab, Tab::Terminal(t) if t.session_id == removed.id));
                if self.active >= self.tabs.len() {
                    self.active = self.tabs.len().saturating_sub(1);
                }
                if self.selected.as_deref() == Some(removed.id.as_str()) {
                    self.selected = self.sessions.first().map(|s| s.id.clone());
                }
                save_sessions(&self.sessions)
                    .map_err(|e| format!("removed in memory, but sessions.json failed: {e}"))?;
                self.toast(ToastKind::Info, format!("MCP: session '{}' deleted", removed.name));
                Ok(json!({ "deleted": { "name": removed.name } }))
            }
            mcp::Action::ListTunnels => {
                let list: Vec<_> = self
                    .standalone_tunnels
                    .iter()
                    .map(|t| self.tunnel_json(t))
                    .collect();
                Ok(json!({ "count": list.len(), "tunnels": list }))
            }
            mcp::Action::GetTunnel { name } => {
                let idx = self.find_tunnel_by(name).ok_or_else(|| {
                    format!("no tunnel named '{name}' — call culvert_list_tunnels first")
                })?;
                Ok(json!({ "tunnel": self.tunnel_json(&self.standalone_tunnels[idx]) }))
            }
            mcp::Action::CreateTunnel { spec } => {
                let name = spec.name.clone().unwrap_or_default().trim().to_string();
                if name.is_empty() {
                    return Err("'name' is required".to_string());
                }
                if self
                    .standalone_tunnels
                    .iter()
                    .any(|t| t.name.eq_ignore_ascii_case(&name))
                {
                    return Err(format!("a tunnel named '{name}' already exists"));
                }
                let listen_port = spec
                    .listen_port
                    .ok_or("'listen_port' is required for a new tunnel")?;
                let mut t = StandaloneTunnel {
                    id: uuid::Uuid::new_v4().to_string(),
                    name,
                    host: spec.host.clone().unwrap_or_default().trim().to_string(),
                    port: spec.port.unwrap_or(22),
                    username: spec.username.clone().unwrap_or_default().trim().to_string(),
                    auth_type: if spec.auth.as_deref() == Some("key") {
                        AuthType::Key
                    } else {
                        AuthType::Password
                    },
                    password: spec.password.clone().unwrap_or_default(),
                    key_path: spec.key_path.clone().unwrap_or_default().trim().to_string(),
                    key_passphrase: spec.key_passphrase.clone().unwrap_or_default(),
                    forwards: Vec::new(),
                };
                let mut forward = PortForward {
                    id: uuid::Uuid::new_v4().to_string(),
                    fwd_type: forward_kind(spec.kind.as_deref()),
                    listen_host: non_empty(spec.listen_host.as_deref(), "127.0.0.1"),
                    listen_port,
                    dest_host: non_empty(spec.dest_host.as_deref(), "127.0.0.1"),
                    dest_port: spec.dest_port.unwrap_or(0),
                    auto_reconnect: spec.auto_reconnect.unwrap_or(false),
                };
                if forward.fwd_type == ForwardType::Dynamic {
                    forward.dest_host.clear();
                    forward.dest_port = 0;
                }
                forward.validate()?;
                t.validate()?;
                t.forwards = vec![forward];
                let id = t.id.clone();
                let tn = t.name.clone();
                self.standalone_tunnels.push(t);
                save_tunnels(&self.standalone_tunnels)
                    .map_err(|e| format!("kept in memory, but tunnels.json failed: {e}"))?;
                self.toast(ToastKind::Success, format!("MCP: tunnel '{tn}' created"));
                Ok(json!({ "created": { "id": id, "name": tn } }))
            }
            mcp::Action::UpdateTunnel { name, spec } => {
                let idx = self.find_tunnel_by(name).ok_or_else(|| {
                    format!("no tunnel named '{name}' — call culvert_list_tunnels first")
                })?;
                let mut t = self.standalone_tunnels[idx].clone();
                let was = t.name.clone();
                if let Some(v) = &spec.name {
                    if !v.trim().is_empty() {
                        t.name = v.trim().to_string();
                    }
                }
                if let Some(v) = &spec.host {
                    t.host = v.trim().to_string();
                }
                if let Some(v) = spec.port {
                    t.port = v;
                }
                if let Some(v) = &spec.username {
                    t.username = v.trim().to_string();
                }
                if let Some(v) = &spec.auth {
                    t.auth_type = if v == "key" {
                        AuthType::Key
                    } else {
                        AuthType::Password
                    };
                }
                if let Some(v) = &spec.password {
                    if !v.is_empty() {
                        t.password = v.clone();
                    }
                }
                if let Some(v) = &spec.key_path {
                    t.key_path = v.trim().to_string();
                }
                if let Some(v) = &spec.key_passphrase {
                    t.key_passphrase = v.clone();
                }
                let mut forward = t.forwards.first().cloned().unwrap_or_default();
                if let Some(v) = &spec.kind {
                    forward.fwd_type = forward_kind(Some(v));
                }
                if let Some(v) = &spec.listen_host {
                    forward.listen_host = non_empty(Some(v), "127.0.0.1");
                }
                if let Some(v) = spec.listen_port {
                    forward.listen_port = v;
                }
                if let Some(v) = &spec.dest_host {
                    forward.dest_host = v.trim().to_string();
                }
                if let Some(v) = spec.dest_port {
                    forward.dest_port = v;
                }
                if let Some(v) = spec.auto_reconnect {
                    forward.auto_reconnect = v;
                }
                if forward.fwd_type == ForwardType::Dynamic {
                    forward.dest_host.clear();
                    forward.dest_port = 0;
                }
                forward.validate()?;
                t.validate()?;
                t.forwards = vec![forward];
                let now = t.name.clone();
                self.standalone_tunnels[idx] = t;
                save_tunnels(&self.standalone_tunnels)
                    .map_err(|e| format!("kept in memory, but tunnels.json failed: {e}"))?;
                self.toast(ToastKind::Info, format!("MCP: tunnel '{now}' updated"));
                Ok(json!({ "updated": { "name": now, "was": was } }))
            }
            mcp::Action::DeleteTunnel { name } => {
                let idx = self.find_tunnel_by(name).ok_or_else(|| {
                    format!("no tunnel named '{name}' — call culvert_list_tunnels first")
                })?;
                let removed = self.standalone_tunnels[idx].clone();
                for f in &removed.forwards {
                    self.tunnels.stop(&standalone_key(&removed.id, &f.id));
                }
                self.standalone_tunnels.remove(idx);
                save_tunnels(&self.standalone_tunnels)
                    .map_err(|e| format!("removed in memory, but tunnels.json failed: {e}"))?;
                self.toast(
                    ToastKind::Info,
                    format!("MCP: tunnel '{}' deleted", removed.name),
                );
                Ok(json!({ "deleted": { "name": removed.name } }))
            }
            mcp::Action::StartTunnel { name } => {
                let idx = self.find_tunnel_by(name).ok_or_else(|| {
                    format!("no tunnel named '{name}' — call culvert_list_tunnels first")
                })?;
                let t = self.standalone_tunnels[idx].clone();
                let forward = t
                    .forwards
                    .first()
                    .cloned()
                    .ok_or("this tunnel has no forward to start")?;
                if self.tunnels.is_running(&standalone_key(&t.id, &forward.id)) {
                    return Err(format!("tunnel '{}' is already running", t.name));
                }
                let key = standalone_key(&t.id, &forward.id);
                self.tunnels.start(&t.to_ssh_session(), &forward)?;
                self.toast(ToastKind::Info, format!("MCP: tunnel '{}' connecting", t.name));
                // `start` only spawns the worker; SSH is still connecting.
                // Callers should poll culvert_list_tunnels for state/error.
                Ok(json!({
                    "starting": t.name,
                    "state": match self.tunnels.phase(&key) {
                        Some(TunnelPhase::Running) => "running",
                        Some(TunnelPhase::Reconnecting) => "reconnecting",
                        _ => "starting",
                    },
                    "note": "SSH runs in the background; poll culvert_list_tunnels for state and last_error",
                }))
            }
            mcp::Action::StopTunnel { name } => {
                let idx = self.find_tunnel_by(name).ok_or_else(|| {
                    format!("no tunnel named '{name}' — call culvert_list_tunnels first")
                })?;
                let t = self.standalone_tunnels[idx].clone();
                let mut stopped = 0usize;
                for f in &t.forwards {
                    let key = standalone_key(&t.id, &f.id);
                    if self.tunnels.is_running(&key) {
                        self.tunnels.stop(&key);
                        stopped += 1;
                    }
                }
                if stopped == 0 {
                    return Err(format!("tunnel '{}' is not running", t.name));
                }
                self.toast(ToastKind::Info, format!("MCP: tunnel '{}' stopped", t.name));
                Ok(json!({ "stopped": t.name }))
            }
            mcp::Action::ListForwards { session } => {
                let targets: Vec<&SshSession> = match session {
                    Some(name) => {
                        let idx = self.find_session_by(name).ok_or_else(|| {
                            format!("no session named '{name}' — call culvert_list_sessions first")
                        })?;
                        vec![&self.sessions[idx]]
                    }
                    None => self.sessions.iter().collect(),
                };
                let mut list = Vec::new();
                for s in targets {
                    for f in &s.forwards {
                        let mut v = self.forward_json(&forward_key(&s.id, &f.id), f);
                        v["session"] = json!(s.name);
                        v["session_id"] = json!(s.id);
                        list.push(v);
                    }
                }
                Ok(json!({ "count": list.len(), "forwards": list }))
            }
            mcp::Action::CreateForward { session, spec } => {
                let idx = self.find_session_by(session).ok_or_else(|| {
                    format!("no session named '{session}' — call culvert_list_sessions first")
                })?;
                if !matches!(self.sessions[idx].protocol, Protocol::Ssh) {
                    return Err(format!(
                        "session '{}' is RDP, not SSH — forwards need an SSH connection",
                        self.sessions[idx].name
                    ));
                }
                let listen_port = spec
                    .listen_port
                    .ok_or("'listen_port' is required for a new forward")?;
                let mut f = PortForward {
                    id: uuid::Uuid::new_v4().to_string(),
                    fwd_type: forward_kind(spec.kind.as_deref()),
                    listen_host: non_empty(spec.listen_host.as_deref(), "127.0.0.1"),
                    listen_port,
                    dest_host: non_empty(spec.dest_host.as_deref(), "127.0.0.1"),
                    dest_port: spec.dest_port.unwrap_or(0),
                    auto_reconnect: spec.auto_reconnect.unwrap_or(false),
                };
                if f.fwd_type == ForwardType::Dynamic {
                    f.dest_host.clear();
                    f.dest_port = 0;
                }
                f.validate()?;
                let sname = self.sessions[idx].name.clone();
                self.sessions[idx].forwards.push(f.clone());
                save_sessions(&self.sessions)
                    .map_err(|e| format!("kept in memory, but sessions.json failed: {e}"))?;
                self.toast(
                    ToastKind::Success,
                    format!("MCP: forward :{} added to '{sname}'", f.listen_port),
                );
                Ok(json!({
                    "created": {
                        "session": sname,
                        "id": f.id,
                        "listen_port": f.listen_port,
                    }
                }))
            }
            mcp::Action::UpdateForward {
                session,
                forward,
                spec,
            } => {
                let sidx = self.find_session_by(session).ok_or_else(|| {
                    format!("no session named '{session}' — call culvert_list_sessions first")
                })?;
                let fidx = self.find_forward(&self.sessions[sidx], forward).ok_or_else(|| {
                    format!(
                        "no forward '{forward}' on session '{}' — call culvert_list_forwards first",
                        self.sessions[sidx].name
                    )
                })?;
                let mut f = self.sessions[sidx].forwards[fidx].clone();
                if let Some(v) = &spec.kind {
                    f.fwd_type = forward_kind(Some(v));
                }
                if let Some(v) = &spec.listen_host {
                    f.listen_host = non_empty(Some(v), "127.0.0.1");
                }
                if let Some(v) = spec.listen_port {
                    f.listen_port = v;
                }
                if let Some(v) = &spec.dest_host {
                    f.dest_host = v.trim().to_string();
                }
                if let Some(v) = spec.dest_port {
                    f.dest_port = v;
                }
                if let Some(v) = spec.auto_reconnect {
                    f.auto_reconnect = v;
                }
                if f.fwd_type == ForwardType::Dynamic {
                    f.dest_host.clear();
                    f.dest_port = 0;
                }
                if f.dest_host.trim().is_empty() && f.fwd_type != ForwardType::Dynamic {
                    f.dest_host = "127.0.0.1".to_string();
                }
                f.validate()?;
                let key = forward_key(&self.sessions[sidx].id, &f.id);
                let was_running = self.tunnels.is_running(&key);
                let sname = self.sessions[sidx].name.clone();
                let fid = f.id.clone();
                let port = f.listen_port;
                self.sessions[sidx].forwards[fidx] = f;
                save_sessions(&self.sessions)
                    .map_err(|e| format!("kept in memory, but sessions.json failed: {e}"))?;
                if was_running {
                    self.tunnels.stop(&key);
                }
                self.toast(
                    ToastKind::Info,
                    format!("MCP: forward :{port} on '{sname}' updated"),
                );
                Ok(json!({
                    "updated": { "session": sname, "id": fid, "listen_port": port },
                    "stopped_because_running": was_running,
                }))
            }
            mcp::Action::DeleteForward { session, forward } => {
                let sidx = self.find_session_by(session).ok_or_else(|| {
                    format!("no session named '{session}' — call culvert_list_sessions first")
                })?;
                let fidx = self.find_forward(&self.sessions[sidx], forward).ok_or_else(|| {
                    format!(
                        "no forward '{forward}' on session '{}' — call culvert_list_forwards first",
                        self.sessions[sidx].name
                    )
                })?;
                let sname = self.sessions[sidx].name.clone();
                let removed = self.sessions[sidx].forwards.remove(fidx);
                let key = forward_key(&self.sessions[sidx].id, &removed.id);
                if self.tunnels.is_running(&key) {
                    self.tunnels.stop(&key);
                }
                save_sessions(&self.sessions)
                    .map_err(|e| format!("removed in memory, but sessions.json failed: {e}"))?;
                self.toast(
                    ToastKind::Info,
                    format!("MCP: forward :{} on '{sname}' deleted", removed.listen_port),
                );
                Ok(json!({
                    "deleted": {
                        "session": sname,
                        "id": removed.id,
                        "listen_port": removed.listen_port,
                    }
                }))
            }
            mcp::Action::StartForward { session, forward } => {
                let sidx = self.find_session_by(session).ok_or_else(|| {
                    format!("no session named '{session}' — call culvert_list_sessions first")
                })?;
                let fidx = self.find_forward(&self.sessions[sidx], forward).ok_or_else(|| {
                    format!(
                        "no forward '{forward}' on session '{}' — call culvert_list_forwards first",
                        self.sessions[sidx].name
                    )
                })?;
                let f = self.sessions[sidx].forwards[fidx].clone();
                let key = forward_key(&self.sessions[sidx].id, &f.id);
                if self.tunnels.is_running(&key) {
                    return Err(format!(
                        "forward :{} on '{}' is already running",
                        f.listen_port, self.sessions[sidx].name
                    ));
                }
                let sname = self.sessions[sidx].name.clone();
                self.tunnels.start(&self.sessions[sidx], &f)?;
                self.toast(
                    ToastKind::Info,
                    format!("MCP: forward :{} on '{sname}' connecting", f.listen_port),
                );
                Ok(json!({
                    "starting": { "session": sname, "id": f.id, "listen_port": f.listen_port },
                    "state": match self.tunnels.phase(&key) {
                        Some(TunnelPhase::Running) => "running",
                        Some(TunnelPhase::Reconnecting) => "reconnecting",
                        _ => "starting",
                    },
                    "note": "SSH runs in the background; poll culvert_list_forwards for state and last_error",
                }))
            }
            mcp::Action::StopForward { session, forward } => {
                let sidx = self.find_session_by(session).ok_or_else(|| {
                    format!("no session named '{session}' — call culvert_list_sessions first")
                })?;
                let fidx = self.find_forward(&self.sessions[sidx], forward).ok_or_else(|| {
                    format!(
                        "no forward '{forward}' on session '{}' — call culvert_list_forwards first",
                        self.sessions[sidx].name
                    )
                })?;
                let f = self.sessions[sidx].forwards[fidx].clone();
                let key = forward_key(&self.sessions[sidx].id, &f.id);
                if !self.tunnels.is_running(&key) {
                    return Err(format!(
                        "forward :{} on '{}' is not running",
                        f.listen_port, self.sessions[sidx].name
                    ));
                }
                self.tunnels.stop(&key);
                let sname = self.sessions[sidx].name.clone();
                self.toast(
                    ToastKind::Info,
                    format!("MCP: forward :{} on '{sname}' stopped", f.listen_port),
                );
                Ok(json!({
                    "stopped": { "session": sname, "id": f.id, "listen_port": f.listen_port }
                }))
            }
            mcp::Action::OpenSession { name } => {
                let idx = self.find_session_by(name).ok_or_else(|| {
                    format!("no session named '{name}' — call culvert_list_sessions first")
                })?;
                let (id, sname) = (
                    self.sessions[idx].id.clone(),
                    self.sessions[idx].name.clone(),
                );
                self.open_session(ctx, &id);
                Ok(json!({ "opened": sname }))
            }
        }
    }

    fn find_session_by(&self, key: &str) -> Option<usize> {
        let key = key.trim();
        self.sessions
            .iter()
            .position(|s| s.id == key)
            .or_else(|| {
                self.sessions
                    .iter()
                    .position(|s| s.name.eq_ignore_ascii_case(key))
            })
    }

    fn find_tunnel_by(&self, key: &str) -> Option<usize> {
        let key = key.trim();
        self.standalone_tunnels
            .iter()
            .position(|t| t.id == key)
            .or_else(|| {
                self.standalone_tunnels
                    .iter()
                    .position(|t| t.name.eq_ignore_ascii_case(key))
            })
    }

    /// A session-bound forward is addressed by its id or its listen port.
    fn find_forward(&self, s: &SshSession, key: &str) -> Option<usize> {
        let key = key.trim();
        s.forwards
            .iter()
            .position(|f| f.id == key)
            .or_else(|| {
                key.parse::<u16>()
                    .ok()
                    .and_then(|port| s.forwards.iter().position(|f| f.listen_port == port))
            })
    }

    /// Groups are addressed by id or name; returns the id.
    fn resolve_group(&self, key: &str) -> Result<String, String> {
        let key = key.trim();
        if let Some(g) = self
            .groups
            .iter()
            .find(|g| g.id == key || g.name.eq_ignore_ascii_case(key))
        {
            return Ok(g.id.clone());
        }
        let names: Vec<&str> = self.groups.iter().map(|g| g.name.as_str()).collect();
        Err(format!(
            "unknown group '{key}' — known groups: {}",
            if names.is_empty() {
                "(none)".to_string()
            } else {
                names.join(", ")
            }
        ))
    }

    /// Public shape of a session. Never includes the password or passphrase —
    /// only whether they exist.
    fn session_json(&self, s: &SshSession) -> serde_json::Value {
        let group = s
            .group
            .as_ref()
            .and_then(|id| self.groups.iter().find(|g| &g.id == id))
            .map(|g| g.name.clone());
        let forwards: Vec<_> = s
            .forwards
            .iter()
            .map(|f| self.forward_json(&forward_key(&s.id, &f.id), f))
            .collect();
        let forward_count = forwards.len();
        serde_json::json!({
            "id": s.id,
            "name": s.name,
            "host": s.host,
            "port": s.port,
            "username": s.username,
            "protocol": match s.protocol { Protocol::Ssh => "ssh", Protocol::Rdp => "rdp" },
            "group": group,
            "auth": match s.auth_type { AuthType::Password => "password", AuthType::Key => "key" },
            "key_path": s.key_path,
            "has_password": !s.password.is_empty(),
            "has_key_passphrase": !s.key_passphrase.is_empty(),
            "forward_count": forward_count,
            "forwards": forwards,
        })
    }

    /// One forward with its live state. Passwords live on the owning session or
    /// standalone tunnel, never here.
    fn forward_json(&self, key: &str, f: &PortForward) -> serde_json::Value {
        let phase = self.tunnels.phase(key);
        let mut value = serde_json::json!({
            "id": f.id,
            "kind": match f.fwd_type {
                ForwardType::Local => "local",
                ForwardType::Remote => "remote",
                ForwardType::Dynamic => "dynamic",
            },
            "listen_host": f.listen_host,
            "listen_port": f.listen_port,
            "dest_host": f.dest_host,
            "dest_port": f.dest_port,
            "auto_reconnect": f.auto_reconnect,
            "running": phase.is_some(),
            "state": match phase {
                None => "stopped",
                Some(TunnelPhase::Starting) => "starting",
                Some(TunnelPhase::Running) => "running",
                Some(TunnelPhase::Reconnecting) => "reconnecting",
            },
        });
        if phase.is_none() {
            if let Some(err) = self.tunnels.last_error(key) {
                value["last_error"] = serde_json::Value::String(err);
            }
        }
        value
    }

    fn tunnel_json(&self, t: &StandaloneTunnel) -> serde_json::Value {
        let forwards: Vec<_> = t
            .forwards
            .iter()
            .map(|f| self.forward_json(&standalone_key(&t.id, &f.id), f))
            .collect();
        serde_json::json!({
            "id": t.id,
            "name": t.name,
            "host": t.host,
            "port": t.port,
            "username": t.username,
            "auth": match t.auth_type { AuthType::Password => "password", AuthType::Key => "key" },
            "key_path": t.key_path,
            "has_password": !t.password.is_empty(),
            "forwards": forwards,
        })
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
        // Switching SSH↔RDP wipes tunnel ties implicitly: forwards are kept
        // but only apply to SSH sessions (RDP shells don't use them).
        session.protocol = ed.protocol.clone();
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
        if ed.standalone {
            // Tunnel with its own SSH endpoint — no saved session involved.
            let host = ed.host.trim().to_string();
            if host.is_empty() {
                ed.error = Some("SSH host is required".to_string());
                return;
            }
            if ed.username.trim().is_empty() {
                ed.error = Some("SSH username is required".to_string());
                return;
            }
            match ed.auth_type {
                AuthType::Password => {
                    if ed.password.is_empty() {
                        ed.error = Some("Password is required for password auth".to_string());
                        return;
                    }
                }
                AuthType::Key => {
                    if ed.key_path.trim().is_empty() {
                        ed.error = Some("Private key file is required for key auth".to_string());
                        return;
                    }
                }
            }
            let tunnel = StandaloneTunnel {
                id: ed
                    .tunnel_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                name: ed.name.trim().to_string(),
                host,
                port: 22, // overwritten right below
                username: ed.username.trim().to_string(),
                auth_type: ed.auth_type.clone(),
                password: ed.password.clone(),
                key_path: ed.key_path.trim().to_string(),
                key_passphrase: String::new(),
                forwards: vec![forward],
            };
            let mut tunnel = tunnel;
            match ed.ssh_port.trim().parse::<u16>() {
                Ok(p) if p > 0 => tunnel.port = p,
                _ => {
                    ed.error = Some("SSH port must be a number between 1 and 65535".to_string());
                    return;
                }
            }
            if let Err(e) = tunnel.validate() {
                ed.error = Some(e);
                return;
            }
            match self
                .standalone_tunnels
                .iter()
                .position(|t| t.id == tunnel.id)
            {
                Some(i) => self.standalone_tunnels[i] = tunnel,
                None => self.standalone_tunnels.push(tunnel),
            }
            self.tunnel_editor = None;
            self.persist_tunnels("Tunnel saved");
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
        // Standalone tunnels run under the synthetic `tunnel-<id>` session
        // key, so a plain forward_key would leave the process listening.
        let key = if self.sessions.iter().any(|s| s.id == session_id) {
            forward_key(session_id, forward_id)
        } else {
            standalone_key(session_id, forward_id)
        };
        self.tunnels.stop(&key);
        self.tunnels.forget_error(&key);
        if let Some(session) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            session.forwards.retain(|f| f.id != forward_id);
            self.persist("Tunnel deleted");
            return;
        }
        if let Some(tunnel) = self
            .standalone_tunnels
            .iter_mut()
            .find(|t| t.id == session_id)
        {
            tunnel.forwards.retain(|f| f.id != forward_id);
            // A standalone tunnel carries exactly one forward; drop the
            // empty shell entirely.
            if tunnel.forwards.is_empty() {
                self.standalone_tunnels
                    .retain(|t| t.id != session_id);
            }
            self.persist_tunnels("Tunnel deleted");
        }
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
    // -----------------------------------------------------------------------
    // Menu bar (MobaXterm-style top menu strip)
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Custom title bar (the OS window is undecorated — see main.rs)
    // -----------------------------------------------------------------------

    /// Top window chrome: brand mark + title on the left, window controls on
    /// the right. The space between them is the drag handle; double-clicking
    /// it toggles maximize, matching native window behavior. This replaces the
    /// Win11 caption, whose invisible resize/shadow border showed as dark bars
    /// at the window edges (issue #1).
    fn titlebar(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));

        // Drag handle over the whole strip; the control buttons are added
        // afterwards so they take interaction priority over it.
        let drag = ui.interact(
            ui.max_rect(),
            ui.id().with("titlebar-drag"),
            Sense::click_and_drag(),
        );
        if drag.drag_started() {
            ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if drag.double_clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        }

        let mark_texture = self
            .mark_texture
            .get_or_insert_with(|| theme::brand_texture(ctx, 128))
            .clone();

        ui.horizontal_centered(|ui| {
            ui.add_space(10.0);
            paint_mark(ui, &mark_texture, 14.0);
            ui.add_space(5.0);
            ui.label(RichText::new("Culvert").size(12.5).strong());
            ui.add_space(5.0);
            ui.label(
                RichText::new(version::tag())
                    .size(10.5)
                    .color(theme::TEXT_FAINT()),
            );

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if window_button(ui, WinBtn::Close).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                let max_btn = if maximized { WinBtn::Restore } else { WinBtn::Max };
                if window_button(ui, max_btn).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
                if window_button(ui, WinBtn::Min).clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
            });
        });
    }

    fn menu_bar(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.spacing_mut().button_padding = egui::vec2(8.0, 2.0);

            // -- Terminal -----------------------------------------------------
            ui.menu_button("Terminal", |menu| {
                let mut copied = false;
                let mut pasted = false;
                let mut cleared = false;
                let mut reconnected = false;
                let mut closed = false;
                let mut connected = false;
                if let Some(Tab::Terminal(t)) = self.tabs.get_mut(self.active) {
                    connected = true;
                    if menu.button("Copy screen").clicked() {
                        let text = t.visible_text();
                        if !text.trim().is_empty() {
                            menu.ctx().copy_text(text);
                            copied = true;
                        }
                        menu.close();
                    }
                    if menu.button("Paste").clicked() {
                        let text = arboard::Clipboard::new()
                            .and_then(|mut c| c.get_text())
                            .unwrap_or_default();
                        if !text.trim().is_empty() {
                            t.paste_text(&text);
                            pasted = true;
                        }
                        menu.close();
                    }
                    if menu.button("Clear screen").clicked() {
                        t.clear_screen();
                        cleared = true;
                        menu.close();
                    }
                    menu.separator();
                    if menu.button("Reconnect shell").clicked() {
                        t.reconnect();
                        reconnected = true;
                        menu.close();
                    }
                    menu.separator();
                    if menu.button("Close tab").clicked() {
                        closed = true;
                        menu.close();
                    }
                }
                if !connected {
                    menu.label(
                        RichText::new("No shell open — connect a session first.")
                            .weak(),
                    );
                }
                // Act after the borrow ends.
                if copied {
                    self.toast(ToastKind::Success, "Screen copied to clipboard");
                }
                if pasted {
                    self.toast(ToastKind::Success, "Pasted into shell");
                }
                if cleared {
                    self.toast(ToastKind::Success, "Screen cleared");
                }
                if reconnected {
                    self.toast(ToastKind::Info, "Reconnecting…");
                }
                if closed {
                    if self.active > 0 {
                        self.close_tab(self.active);
                    } else {
                        self.toast(ToastKind::Info, "That tab is not closable");
                    }
                }
            });

            // -- Sessions -----------------------------------------------------
            ui.menu_button("Sessions", |menu| {
                if menu.button("New session…  (Ctrl+N)").clicked() {
                    self.session_editor = Some(SessionEditor::new());
                    menu.close();
                }
                if menu.button("New tunnel…  (Ctrl+T)").clicked() {
                    self.tunnel_editor = Some(TunnelEditor::new_standalone());
                    menu.close();
                }
                if !self.sessions.is_empty() {
                    menu.separator();
                    let mut open_id: Option<String> = None;
                    for s in self.sessions.iter().take(12) {
                        if menu.button(RichText::new(&s.name).size(12.0)).clicked() {
                            open_id = Some(s.id.clone());
                            menu.close();
                        }
                    }
                    if let Some(id) = open_id {
                        self.open_session(ctx, &id);
                    }
                }
                if menu.button("Quick connect (sidebar)").clicked() {
                    menu.close();
                }
            });

            // -- View ---------------------------------------------------------
            ui.menu_button("View", |menu| {
                if menu.checkbox(&mut self.split_view, "Split view").changed() {
                    menu.close();
                }
                if menu
                    .checkbox(&mut self.show_logs, "Activity log")
                    .changed()
                {
                    menu.close();
                }
                menu.separator();
                if menu.button("Files tool").clicked() {
                    self.side_mode = SideMode::Files;
                    self.sidebar_collapsed = false;
                    menu.close();
                }
                if menu.button("Sessions tool").clicked() {
                    self.side_mode = SideMode::Sessions;
                    menu.close();
                }
                menu.separator();
                if self.sidebar_collapsed {
                    if menu.button("Expand sidebar").clicked() {
                        self.sidebar_collapsed = false;
                        menu.close();
                    }
                } else if menu.button("Collapse sidebar").clicked() {
                    self.sidebar_collapsed = true;
                    menu.close();
                }
                menu.separator();
                if menu.checkbox(&mut self.ai.open, "AI Copilot").changed() {
                    if self.ai.open && self.ai.needs_setup() {
                        self.ai.open_settings();
                    }
                    menu.close();
                }
                if menu.button("AI Copilot settings…").clicked() {
                    self.ai.open_settings();
                    menu.close();
                }
            });

            // -- Tools ---------------------------------------------------------
            ui.menu_button("Tools", |menu| {
                if menu.button("Snippet manager…").clicked() {
                    self.snippets_backup = self.snippets.clone();
                    self.snippets_open = true;
                    menu.close();
                }
                menu.separator();
                if menu.button("Files browser").clicked() {
                    self.side_mode = SideMode::Files;
                    self.sidebar_collapsed = false;
                    menu.close();
                }
                menu.separator();
                if menu.button("MCP server…").clicked() {
                    self.mcp_modal = true;
                    menu.close();
                }
            });

            // -- Macros --------------------------------------------------------
            ui.menu_button("Macros", |menu| {
                if menu.button("Manage snippets…").clicked() {
                    self.snippets_backup = self.snippets.clone();
                    self.snippets_open = true;
                    menu.close();
                }
                if !self.snippets.is_empty() {
                    menu.separator();
                    for s in self.snippets.clone() {
                        if menu.button(RichText::new(&s.name).size(12.0)).clicked() {
                            menu.close();
                            if let Some(Tab::Terminal(t)) = self.tabs.get_mut(self.active) {
                                let mut bytes = s.command.as_bytes().to_vec();
                                if !s.command.ends_with('\n') {
                                    bytes.push(b'\r');
                                }
                                t.send_bytes(bytes);
                                self.toast(
                                    ToastKind::Success,
                                    format!("Macro sent · {}", s.name),
                                );
                            } else {
                                self.toast(
                                    ToastKind::Info,
                                    "Open a session shell to run macros",
                                );
                            }
                        }
                    }
                }
            });

            // -- Help ----------------------------------------------------------
            ui.menu_button("Help", |menu| {
                if menu.button(format!("About Culvert {}", version::tag())).clicked() {
                    self.toast(
                        ToastKind::Info,
                        format!(
                            "Culvert {} · {} · MIT · SSH, tunnels and an AI Copilot in one exe",
                            version::tag(),
                            version::display()
                        ),
                    );
                    menu.close();
                }
                if menu.button("Open project page").clicked() {
                    let _ = std::process::Command::new("explorer")
                        .arg("https://github.com/Shinjiru-stack/Culvert")
                        .spawn();
                    menu.close();
                }
            });

            // Right-aligned quiet build tag.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(10.0);
                ui.label(
                    RichText::new(version::tag())
                        .size(9.5)
                        .color(theme::TEXT_FAINT()),
                );
            });
        });
    }

    pub fn ui(&mut self, ctx: &egui::Context) {
        // One-shot CLI autostart (`--open <session>` + `--files`): open the
        // named saved session and optionally flip the sidebar to its file
        // browser. Runs on the first frame once sessions are loaded.
        if let Some((name, files)) = self.startup.take() {
            let found = self.sessions.iter().find(|s| {
                s.name.eq_ignore_ascii_case(&name) || s.name.to_lowercase().contains(&name.to_lowercase())
            });
            if let Some(s) = found.cloned() {
                self.open_session(ctx, &s.id);
                if files {
                    self.side_mode = SideMode::Files;
                }
            } else {
                self.toast(
                    ToastKind::Error,
                    format!("No saved session matches \"{}\"", name),
                );
            }
        }

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

        // Terminal overlay buttons + files-pane lifecycle (session tabs now
        // carry both the shell and the file browser).
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

        // Docked file browser (sidebar "Files" tool): drain its events and
        // honor the Close (✕) press from the browser overlay.
        let mut dock_close = false;
        if let Some((_, fb)) = self.docked.as_mut() {
            if fb.poll() {
                ctx.request_repaint();
            }
            dock_close = fb.want_close;
        }
        if dock_close {
            self.close_all_docked();
        }
        if let Some(i) = close_idx {
            self.close_tab(i);
        }

        // AI Copilot: start the engine on first frame, publish this frame's
        // context snapshot, drain engine events and execute any UI action it
        // asked for (the engine itself never touches app state).
        self.ai.ensure_engine(ctx);
        {
            let snap = self.ai_snapshot();
            if let Ok(mut w) = self.ai.snapshot.write() {
                *w = snap;
            }
        }
        if self.ai.poll() {
            ctx.request_repaint();
        }
        if let Some((id, action, _label)) = self.ai.take_action() {
            if let Some(result) = self.ai_apply_action(ctx, id, &action) {
                self.ai.complete_action(id, result);
            }
            ctx.request_repaint();
        }
        self.tick_ai_terminal(ctx);
        self.tick_mcp(ctx);
        if self.ai.settings_modal(ctx) {
            self.toast(ToastKind::Success, "AI Copilot settings saved");
        }

        // Global shortcuts (never while a shell has the keyboard).
        if !self.terminal_focused(ctx) {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::N)) {
                self.session_editor = Some(SessionEditor::new());
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::T)) {
                self.tunnel_editor = Some(TunnelEditor::new_standalone());
            }
        }

        // Ctrl+Tab / Ctrl+Shift+Tab: switch tabs even while a shell has the
        // keyboard. Consume it before the terminal widget reads input so no
        // stray Tab reaches the remote process. Check the Shift variant
        // first: consume_key ignores extra Shift/Alt modifiers.
        let tab_step = if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::Tab,
            )
        }) {
            Some(-1)
        } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Tab)) {
            Some(1)
        } else {
            None
        };
        if let Some(step) = tab_step {
            self.cycle_tab(step);
            ctx.request_repaint();
        }

        // F11: give the terminal the whole (real, OS-)full-screen window.
        // Consumed before the terminal draws so a shell application never sees
        // the key. A popped-out shell handles F11 in its own window.
        if self.term_detached.is_none()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F11))
            && matches!(self.tabs.get(self.active), Some(Tab::Terminal(_)))
        {
            self.term_fullscreen = !self.term_fullscreen;
        }
        let focus_term = self.term_fullscreen
            && self.term_detached.is_none()
            && matches!(self.tabs.get(self.active), Some(Tab::Terminal(_)));
        if !matches!(self.tabs.get(self.active), Some(Tab::Terminal(_))) {
            self.term_fullscreen = false;
        }
        // Send the window state only on transitions. If the user leaves full
        // screen another way (OS shortcut, taskbar), follow them back — but
        // only after the OS has confirmed it was full-screen, otherwise the
        // stale `false` on the next frame would undo the command.
        if focus_term != self.term_os_fullscreen {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(focus_term));
            self.term_os_fullscreen = focus_term;
            self.term_os_confirmed = false;
        } else if self.term_os_fullscreen {
            match ctx.input(|i| i.viewport().fullscreen) {
                Some(true) => self.term_os_confirmed = true,
                Some(false) if self.term_os_confirmed => {
                    self.term_fullscreen = false;
                    self.term_os_fullscreen = false;
                    self.term_os_confirmed = false;
                }
                _ => {}
            }
        }

        // Custom title bar (the window is undecorated — see main.rs). Drawn
        // first so it sits above the menu strip; owns window drag + the
        // minimize / maximize / close controls.
        egui::TopBottomPanel::top("titlebar")
            .exact_height(28.0)
            .frame(Frame::NONE.fill(theme::BG_PANEL()))
            .show(ctx, |ui| self.titlebar(ui, ctx));

        if focus_term {
            // Slim strip: what is full-screen, and how to get out.
            egui::TopBottomPanel::top("term_focus_bar")
                .exact_height(26.0)
                .frame(
                    Frame::NONE
                        .fill(theme::BG_PANEL())
                        .stroke(Stroke::new(1.0_f32, theme::BORDER())),
                )
                .show(ctx, |ui| {
                    ui.horizontal_centered(|ui| {
                        ui.add_space(8.0);
                        let title = match self.tabs.get(self.active) {
                            Some(Tab::Terminal(t)) => t.title.clone(),
                            _ => String::new(),
                        };
                        ui.label(
                            RichText::new(format!("{title} — full screen terminal"))
                                .size(11.0)
                                .color(theme::TEXT_DIM()),
                        );
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.add_space(8.0);
                            if ui
                                .button(RichText::new("Minimize").size(11.0))
                                .on_hover_text("Back to the Culvert window (F11)")
                                .clicked()
                            {
                                self.term_fullscreen = false;
                            }
                            if ui
                                .button(RichText::new("Pop out").size(11.0))
                                .on_hover_text("Open this terminal in its own window")
                                .clicked()
                            {
                                self.term_fullscreen = false;
                                self.term_detached_focused = false;
                                // A normal child window cannot rise above a
                                // topmost parent: make sure Culvert is normal.
                                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                                    egui::WindowLevel::Normal,
                                ));
                                if let Some(Tab::Terminal(t)) = self.tabs.get(self.active) {
                                    self.term_detached = Some(t.session_id.clone());
                                }
                            }
                            ui.label(
                                RichText::new("F11 full screen")
                                    .size(10.0)
                                    .color(theme::TEXT_FAINT()),
                            );
                        });
                    });
                });
        }

        if !focus_term {
            egui::TopBottomPanel::top("menubar")
                .exact_height(22.0)
                .frame(
                    Frame::NONE
                        .fill(theme::BG_PANEL())
                        .stroke(Stroke::new(1.0_f32, theme::BORDER())),
                )
                .show(ctx, |ui| self.menu_bar(ui, ctx));

            // Full-width icon toolbar, exactly like MobaXterm's: it sits above the
            // sidebar and the work area rather than inside the content pane.
            egui::TopBottomPanel::top("toolbar")
                .exact_height(58.0)
                .frame(
                    Frame::NONE
                        .fill(theme::BG_PANEL())
                        .stroke(Stroke::new(1.0_f32, theme::BORDER()))
                        .inner_margin(Margin::symmetric(6, 2)),
                )
                .show(ctx, |ui| self.toolbar(ui));

            egui::TopBottomPanel::bottom("statusbar")
                .exact_height(22.0)
                .frame(
                    Frame::NONE
                        .fill(theme::SB_BG())
                        .stroke(Stroke::new(1.0_f32, theme::SB_BG())),
                )
                .show(ctx, |ui| self.statusbar(ui));

            if self.sidebar_collapsed {
                // Compact MobaXterm-style icon rail. Its own panel id, so
                // collapsing never overwrites the width the user dragged the
                // expanded panel to (egui stores width per panel id).
                egui::SidePanel::left("sessions_rail")
                    .exact_width(56.0)
                    .resizable(false)
                    .frame(
                        Frame::NONE
                            .fill(theme::BG_PANEL())
                            .inner_margin(Margin::symmetric(7, 8)),
                    )
                    .show(ctx, |ui| self.sidebar_rail(ui, ctx));
            } else {
                egui::SidePanel::left("sessions")
                    .resizable(true)
                    .default_width(220.0)
                    .width_range(180.0..=380.0)
                    .frame(
                        Frame::NONE
                            .fill(theme::BG_PANEL())
                            .inner_margin(Margin {
                                left: 10,
                                right: 10,
                                top: 12,
                                bottom: 8,
                            }),
                    )
                    .show(ctx, |ui| self.sidebar(ui, ctx));
            }

            // Far-right icon rail (MobaXterm's narrow tool strip).
            egui::SidePanel::right("right_rail")
                .exact_width(30.0)
                .resizable(false)
                .frame(
                    Frame::NONE
                        .fill(theme::BG_PANEL())
                        .stroke(Stroke::new(1.0_f32, theme::BORDER()))
                        .inner_margin(Margin::symmetric(2, 6)),
                )
                .show(ctx, |ui| self.right_rail(ui, ctx));

            // AI Copilot sits between the rail and the workspace; resizable
            // and collapsible like the session sidebar.
            if self.ai.open {
                egui::SidePanel::right("ai_copilot")
                    .resizable(true)
                    .default_width(370.0)
                    .width_range(300.0..=560.0)
                    .frame(
                        Frame::NONE
                            .fill(theme::BG_PANEL())
                            .stroke(Stroke::new(1.0_f32, theme::BORDER()))
                            .inner_margin(Margin {
                                left: 10,
                                right: 10,
                                top: 10,
                                bottom: 8,
                            }),
                    )
                    .show(ctx, |ui| self.ai.panel_ui(ui, ctx));
            }
        }

        egui::CentralPanel::default()
            .frame(Frame::NONE.fill(theme::BG_WINDOW()).inner_margin(Margin::symmetric(12, 8)))
            .show(ctx, |ui| {
                // While split view is showing a grid of shells, the tab
                // strip hides — terminals fill the workspace (panes have
                // their own title bars with close buttons). Full-screen
                // terminal mode hides it too: just the shell.
                let hide_strip =
                    focus_term || (self.split_view && self.terminal_tab_count() >= 2);
                if !hide_strip {
                    self.tab_strip(ui, ctx);
                    ui.add_space(6.0);
                }
                self.workspace(ui, ctx);
            });

        // Popped-out terminal: a real OS window with native minimize / maximize
        // / close. Closing it, or "Pop back in", returns the shell to its tab.
        if let Some(sid) = self.term_detached.clone() {
            let found = self.tabs.iter().position(|t| match t {
                Tab::Terminal(t) => t.session_id == sid,
                _ => false,
            });
            if let Some(idx) = found {
                let mut pop_in = false;
                let mut tab_cycle: Option<isize> = None;
                // Ask the freshly created OS window to come to the front once.
                // Repeating it every frame would steal focus back whenever the
                // user clicks Culvert.
                let needs_focus = !self.term_detached_focused;
                if let Some(Tab::Terminal(term)) = self.tabs.get_mut(idx) {
                    let title = format!("{} — Culvert", term.title);
                    ctx.show_viewport_immediate(
                        egui::ViewportId::from_hash_of("culvert_terminal_window"),
                        egui::ViewportBuilder::default()
                            .with_title(&title)
                            // The main window is undecorated (see main.rs), and
                            // child viewports would inherit that. Ask for the
                            // native frame here: the popped-out terminal needs
                            // real OS minimize / maximize / close buttons.
                            .with_decorations(true)
                            .with_active(true)
                            .with_inner_size([1000.0, 660.0])
                            .with_min_inner_size([460.0, 280.0]),
                        |vctx, _class| {
                            if needs_focus {
                                vctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                            }
                            // Ctrl+Tab here switches the main window's tabs and
                            // brings Culvert forward; consume it before the
                            // shell sees a Tab. Check Shift first: consume_key
                            // ignores extra Shift/Alt.
                            if vctx.input_mut(|i| {
                                i.consume_key(
                                    egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                                    egui::Key::Tab,
                                )
                            }) {
                                tab_cycle = Some(-1);
                            } else if vctx.input_mut(|i| {
                                i.consume_key(egui::Modifiers::COMMAND, egui::Key::Tab)
                            }) {
                                tab_cycle = Some(1);
                            }
                            // F11 toggles this window's own full screen; the
                            // native title bar handles minimize/maximize/close.
                            if vctx.input_mut(|i| {
                                i.consume_key(egui::Modifiers::NONE, egui::Key::F11)
                            }) {
                                let on = !vctx
                                    .input(|i| i.viewport().fullscreen.unwrap_or(false));
                                vctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(on));
                            }
                            egui::TopBottomPanel::top("detached_bar")
                                .exact_height(30.0)
                                .frame(
                                    Frame::NONE
                                        .fill(theme::BG_PANEL())
                                        .stroke(Stroke::new(1.0_f32, theme::BORDER())),
                                )
                                .show(vctx, |ui| {
                                    ui.horizontal_centered(|ui| {
                                        ui.add_space(8.0);
                                        ui.label(
                                            RichText::new(&title)
                                                .size(11.0)
                                                .color(theme::TEXT_DIM()),
                                        );
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                ui.add_space(8.0);
                                                if ui
                                                    .button(
                                                        RichText::new("Pop back in").size(11.0),
                                                    )
                                                    .on_hover_text(
                                                        "Return this shell to its Culvert tab",
                                                    )
                                                    .clicked()
                                                {
                                                    pop_in = true;
                                                }
                                            },
                                        );
                                    });
                                });
                            egui::CentralPanel::default()
                                .frame(Frame::NONE.fill(theme::BG_WINDOW()))
                                .show(vctx, |ui| term.ui(ui));
                            if vctx.input(|i| i.viewport().close_requested()) {
                                pop_in = true;
                            }
                        },
                    );
                }
                if needs_focus {
                    self.term_detached_focused = true;
                }
                if let Some(step) = tab_cycle {
                    self.cycle_tab(step);
                    // The user is browsing tabs now: bring Culvert forward
                    // instead of leaving the pop-out covering the new tab.
                    self.term_detached_focused = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ctx.request_repaint();
                }
                if pop_in {
                    self.term_detached = None;
                    self.term_detached_focused = false;
                    ctx.request_repaint();
                }
            } else {
                // The tab was closed while the window was out.
                self.term_detached = None;
                self.term_detached_focused = false;
            }
        }

        self.session_editor_modal(ctx);
        self.tunnel_editor_modal(ctx);
        self.group_editor_modal(ctx);
        self.confirm_modal(ctx);
        self.logs_modal(ctx);
        self.snippets_modal(ctx);
        self.mcp_modal(ctx);
        self.toast_ui(ctx);

        if any_connecting {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
        // Tunnel rows change in the background (connecting -> running, or a
        // connect failure); keep refreshing until every one has settled.
        if self.tunnels.any_pending() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    // -----------------------------------------------------------------------
    // Sidebar: saved sessions
    // -----------------------------------------------------------------------

    /// Compact icon rail (sidebar collapsed): Sessions / Files tiles plus an
    /// expand control. Clicking a tool tile also expands back out.
    fn sidebar_rail(&mut self, ui: &mut Ui, _ctx: &egui::Context) {
        ui.with_layout(Layout::top_down(Align::Min), |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            for (mode, glyph, hover) in [
                (SideMode::Sessions, tglyph::RAIL_SESSIONS, "Sessions — saved SSH sessions"),
                (SideMode::Files, tglyph::RAIL_FILES, "Files — browse the connected server"),
            ] {
                let active = self.side_mode == mode;
                let resp = rail_tile(ui, glyph, active);
                if resp.clicked() {
                    self.sidebar_collapsed = false;
                    if mode == SideMode::Sessions && self.side_mode != SideMode::Sessions {
                        // Leaving Files mode: no lingering idle SFTP links.
                        self.close_all_docked();
                    }
                    self.side_mode = mode;
                }
                resp.on_hover_text(hover);
            }
            ui.add_space(14.0);
            let resp = rail_tile(ui, tglyph::CHEVRON_RIGHT, false);
            if resp.clicked() {
                self.sidebar_collapsed = false;
            }
            resp.on_hover_text("Expand sidebar");
        });
    }

    fn sidebar(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        // The brand and the Sessions/Files switcher now live in the title bar and
        // the full-width toolbar, so the sidebar starts at the session list.
        ui.add_space(2.0);

        if self.side_mode == SideMode::Files {
            self.sidebar_files(ui, ctx);
            return;
        }

        ui.label(
            RichText::new("Double-click a session to open a session window")
                .size(10.0)
                .color(theme::TEXT_FAINT()),
        );
        ui.add_space(10.0);

        // MobaXterm-style quick connect: type user@host:port and go. Saving
        // the session is optional — connect immediately when a password is
        // on file for the profile, otherwise prefill the session editor.
        let mut quick_go = false;
        ui.horizontal(|ui| {
            // Put the button on the right first, then let the text field fill
            // the remaining width. Sizing the field as "available - N" left it
            // a few pixels too wide (button padding varies), which inflated the
            // panel's content width and made every sidebar resize snap back.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if styled_button(
                    ui,
                    RichText::new(tglyph::PLUG).font(theme::icon(13.0)),
                    true,
                    false,
                    Some(egui::vec2(24.0, 24.0)),
                )
                .on_hover_text("Quick connect")
                .clicked()
                {
                    quick_go = true;
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let w = ui.available_width();
                    let resp = ui
                        .add_sized(
                            [w, 24.0],
                            egui::TextEdit::singleline(&mut self.quick_connect)
                                .hint_text("quick connect: user@host:port")
                                .margin(egui::vec2(8.0, 4.0)),
                        )
                        .on_hover_text(
                            "Quick connect — user@host or user@host:port, then Enter",
                        );
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if resp.lost_focus() && enter {
                        quick_go = true;
                    }
                });
            });
        });
        if quick_go {
            self.quick_connect_go(ctx);
        }
        ui.add_space(8.0);

        let full = egui::vec2(ui.available_width(), 26.0);
        if styled_button(
            ui,
            RichText::new("+  New session").size(12.5),
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
            let (title_bar, _) = ui.allocate_exact_size(egui::vec2(4.0, 16.0), Sense::hover());
            ui.painter().rect_filled(title_bar, 2.0, theme::ACCENT());
            ui.label(RichText::new("Sessions").text_style(theme::h2()).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ghost_button(
                    ui,
                    RichText::new("+ Group").size(10.5).color(theme::TEXT_DIM()),
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

        ui.add(
            egui::TextEdit::singleline(&mut self.search)
                .hint_text("Search sessions…")
                .desired_width(ui.available_width())
                .margin(egui::vec2(6.0, 3.0)),
        );
        ui.add_space(6.0);

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
                            .size(11.0)
                            .color(theme::TEXT_DIM()),
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
                                .size(10.0)
                                .color(theme::TEXT_FAINT()),
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
                GroupCmd::AddSession => {
                    let mut ed = SessionEditor::new();
                    ed.group = Some(gid.clone());
                    self.session_editor = Some(ed);
                }
            }
        }
        for (id, action) in card_actions {
            match action {
                CardAction::Select => self.selected = Some(id.clone()),
                CardAction::Connect | CardAction::OpenShell => self.open_session(ctx, &id),
                // "Browse files": open the session and flip the sidebar to its
                // docked file browser.
                CardAction::BrowseFiles => {
                    self.open_session(ctx, &id);
                    self.side_mode = SideMode::Files;
                }
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

    /// Sidebar "Files" tool: a docked WinSCP-style browser that follows the
    /// focused connected session (an open session tab; the foreground tab
    /// wins), like MobaXterm's scp browser pane.
    fn sidebar_files(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let Some(focused) = self.focused_file_session_id() else {
            // Nothing connected: the docked browser must not linger with an
            // idle SFTP link.
            if self.docked.is_some() {
                self.close_all_docked();
            }
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "No open SSH session — double-click a saved session to\n\
                     connect; the file browser then follows that server.",
                )
                .size(10.0)
                .color(theme::TEXT_FAINT()),
            );
            return;
        };
        let Some(session) = self
            .sessions
            .iter()
            .find(|s| s.id == focused)
            .cloned()
        else {
            // Session deleted while docked — drop its browser too.
            self.close_all_docked();
            return;
        };
        if session.protocol == Protocol::Rdp {
            // No SFTP over RDP; release any stray dock (defensive).
            self.close_all_docked();
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "RDP sessions have no file browser — SFTP runs over SSH.",
                )
                .size(10.5)
                .color(theme::TEXT_FAINT()),
            );
            return;
        }
        // Header: whose files are on display (follows focus each frame).
        ui.horizontal(|ui| {
            let (bar, _) = ui.allocate_exact_size(egui::vec2(4.0, 16.0), Sense::hover());
            ui.painter().rect_filled(bar, 2.0, theme::INFO());
            ui.label(RichText::new("Files").text_style(theme::h2()).strong());
            ui.label(
                RichText::new(&session.name).size(10.0).color(theme::TEXT_FAINT())
            );
        });
        ui.add_space(6.0);
        self.sync_dock_focus(ctx, &focused);
        if let Some((_, fb)) = self.docked.as_mut() {
            fb.ui(ui);
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
        let auth_label = match session.protocol {
            Protocol::Rdp => "RDP",
            _ => match session.auth_type {
                AuthType::Password => "Password",
                AuthType::Key => "Key",
            },
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

    /// Icon toolbar across the top of the workspace (Tabler glyph tiles).
    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if tool_button(ui, tglyph::SESSION, theme::SUCCESS(), "Session", false, "New session")
                .clicked()
            {
                self.session_editor = Some(SessionEditor::new());
            }
            if tool_button(
                ui,
                tglyph::TUNNEL,
                theme::INFO(),
                "Tunnel",
                false,
                "New tunnel — any SSH host; a saved session is optional",
            )
            .clicked()
            {
                self.tunnel_editor = Some(TunnelEditor::new_standalone());
            }
            if tool_button(
                ui,
                tglyph::RAIL_SESSIONS,
                theme::VIOLET(),
                "Sessions",
                !self.sidebar_collapsed && self.side_mode == SideMode::Sessions,
                "Saved sessions",
            )
            .clicked()
            {
                self.sidebar_collapsed = false;
                self.side_mode = SideMode::Sessions;
            }
            if tool_button(
                ui,
                tglyph::RAIL_FILES,
                Color32::from_rgb(18, 134, 127),
                "Files",
                !self.sidebar_collapsed && self.side_mode == SideMode::Files,
                "Browse the connected server",
            )
            .clicked()
            {
                self.sidebar_collapsed = false;
                self.side_mode = SideMode::Files;
            }
            // Slim vertical separator between the action and view groups.
            {
                let (r, _) = ui.allocate_exact_size(egui::vec2(9.0, 34.0), Sense::hover());
                ui.painter().line_segment(
                    [
                        egui::pos2(r.center().x, r.top() + 4.0),
                        egui::pos2(r.center().x, r.bottom() - 4.0),
                    ],
                    Stroke::new(1.0_f32, theme::BORDER_STRONG()),
                );
            }
            let split_active = self.split_view && self.terminal_tab_count() >= 2;
            if tool_button(
                ui,
                tglyph::SPLIT,
                theme::ACCENT(),
                "Split",
                split_active,
                "Toggle split view",
            )
            .clicked()
            {
                self.split_view = !self.split_view;
            }
            if tool_button(
                ui,
                tglyph::ACTIVITY,
                theme::WARNING(),
                "Activity",
                self.show_logs,
                "Activity log",
            )
            .clicked()
            {
                self.show_logs = !self.show_logs;
            }
            if tool_button(
                ui,
                tglyph::COPILOT,
                theme::VIOLET(),
                "Copilot",
                self.ai.open,
                "AI Copilot — control your servers in natural language",
            )
            .clicked()
            {
                self.ai.open = !self.ai.open;
                if self.ai.open && self.ai.needs_setup() {
                    self.ai.open_settings();
                }
            }
        });
    }

    /// Far-right icon rail, like MobaXterm's narrow tool strip: palette
    /// switch at the top, sidebar toggle below it.
    fn right_rail(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        ui.with_layout(Layout::top_down(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            let (glyph, tint, tip) = if theme::is_dark() {
                (tglyph::SUN, theme::WARNING(), "Switch to the light theme")
            } else {
                (tglyph::MOON, theme::INFO(), "Switch to the dark theme")
            };
            if rail_button(ui, glyph, tint, tip).clicked() {
                theme::set_dark(!theme::is_dark());
                theme::apply(ctx);
            }
            if rail_button(
                ui,
                if self.sidebar_collapsed {
                    tglyph::CHEVRON_RIGHT
                } else {
                    tglyph::CHEVRON_LEFT
                },
                theme::TEXT_DIM(),
                if self.sidebar_collapsed {
                    "Show the session sidebar"
                } else {
                    "Hide the session sidebar"
                },
            )
            .clicked()
            {
                self.sidebar_collapsed = !self.sidebar_collapsed;
            }
        });
    }

    fn tab_strip(&mut self, ui: &mut Ui, ctx: &egui::Context) {
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
                match tab {
                    Tab::Terminal(t) => {
                        let dot = match &t.status {
                            TermStatus::Connected => Some(theme::SUCCESS()),
                            TermStatus::Connecting => Some(theme::WARNING()),
                            TermStatus::Closed(_) => Some(theme::DANGER()),
                        };
                        let title = t.title.clone();
                        let (clicked, closed, menu) =
                            tab_chip(ui, &title, dot, self.active == i, true);
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
                    Tab::Tunnels => {}
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
            // + opens a shell for the selected session (quick-connect style).
            let plus_resp = ui.add(
                egui::Button::new(RichText::new("+").size(13.0).color(theme::ACCENT()))
                    .fill(Color32::TRANSPARENT)
                    .stroke(Stroke::NONE)
                    .min_size(egui::vec2(24.0, 24.0)),
            );
            if plus_resp.clicked() {
                match self
                    .selected
                    .clone()
                    .filter(|id| self.sessions.iter().any(|s| &s.id == id))
                {
                    Some(id) => self.open_session(ctx, &id),
                    None => match self.sessions.first().map(|s| s.id.clone()) {
                        Some(id) => self.open_session(ctx, &id),
                        None => {
                            self.toast(ToastKind::Info, "Add a session first — the + opens a session.");
                        }
                    },
                }
            }
            plus_resp.on_hover_text("Open the session for the selected server");
        });
    }

    // -----------------------------------------------------------------------
    // Workspace
    // -----------------------------------------------------------------------

    fn workspace(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let active = self.active;

        // The active shell lives in its own OS window: draw a card instead of
        // the terminal (drawing both would feed the PTY twice).
        if let (Some(sid), Some(Tab::Terminal(t))) =
            (self.term_detached.as_deref(), self.tabs.get(active))
        {
            if t.session_id == sid {
                let title = t.title.clone();
                ui.add_space(28.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(format!("{title} is open in its own window"))
                            .size(15.0)
                            .color(theme::TEXT()),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "Use that window's own minimize / maximize / close controls, or bring it back here.",
                        )
                        .size(11.0)
                        .color(theme::TEXT_DIM()),
                    );
                    ui.add_space(14.0);
                    if ui.button("Bring it back").clicked() {
                        self.term_detached = None;
                        self.term_detached_focused = false;
                    }
                });
                return;
            }
        }

        let use_grid = self.split_view && self.terminal_tab_count() >= 2;
        if use_grid {
            self.terminal_grid(ui, ctx);
            return;
        }
        match self.tabs.get_mut(active) {
            Some(Tab::Terminal(term)) => {
                let snippets = self.snippets.clone();
                let mut manage = false;
                let fullscreen = self.term_fullscreen;
                let session_id = term.session_id.clone();
                let action =
                    terminal_quick_bar(ui, term, &snippets, &mut manage, fullscreen);
                term.ui(ui);
                if manage {
                    self.snippets_backup = self.snippets.clone();
                    self.snippets_open = true;
                }
                match action {
                    QuickAction::None => {}
                    QuickAction::Copied => {
                        self.toast(ToastKind::Success, "Screen copied to clipboard")
                    }
                    QuickAction::ClipboardEmpty => {
                        self.toast(ToastKind::Info, "Clipboard is empty")
                    }
                    QuickAction::Cleared => self.toast(ToastKind::Success, "Screen cleared"),
                    QuickAction::Reconnect => self.toast(ToastKind::Info, "Reconnecting…"),
                    QuickAction::ToggleFullscreen => {
                        self.term_fullscreen = !self.term_fullscreen;
                    }
                    QuickAction::Detach => {
                        self.term_fullscreen = false;
                        self.split_view = false;
                        self.term_detached_focused = false;
                        // Same as the full-screen bar: never let a topmost
                        // main window hide the new terminal window.
                        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                            egui::WindowLevel::Normal,
                        ));
                        self.term_detached = Some(session_id);
                    }
                }
                return;
            }
            _ => {}
        }
        self.tunnels_page(ui, ctx);
    }

    fn terminal_tab_count(&self) -> usize {
        self.tabs
            .iter()
            .filter(|t| matches!(t, Tab::Terminal(_)))
            .count()
    }

    /// Split view: every open shell in a 2-column grid, each pane with its
    /// own title bar (live, individually interactive).
    ///
    /// Geometry is exact: the whole workspace rect is reserved up front and
    /// every pane is placed at an absolutely computed rect via `scope_builder`.
    /// This avoids egui's min-rect/used-size allocation, which would collapse
    /// the painter-only terminal panes and reflow the grid.
    fn terminal_grid(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let area = ui.available_rect_before_wrap();
        let gap = 8.0;
        let ratio = self.split_ratio.clamp(0.25, 0.75);
        let w0 = (area.width() - gap) * ratio;
        let w1 = (area.width() - gap) * (1.0 - ratio);
        let cell_h = ((area.height() - gap) / 2.0).max(150.0);
        let idxs: Vec<usize> = self
            .tabs
            .iter()
            .enumerate()
            .filter(|(_, t)| matches!(t, Tab::Terminal(_)))
            .map(|(i, _)| i)
            .collect();

        // Reserve the whole workspace once; panes are painted at absolute
        // rects afterwards, so nothing after this point reflows.
        ui.allocate_exact_size(area.size(), Sense::hover());

        for (n, &i) in idxs.iter().enumerate() {
            let col = n % 2;
            let row = n / 2;
            let x = if col == 0 {
                area.left()
            } else {
                area.left() + w0 + gap
            };
            let cw = if col == 0 { w0 } else { w1 };
            let pos = egui::pos2(x, area.top() + (row as f32) * (cell_h + gap));
            let cell = egui::vec2((cw - 2.0).max(220.0), cell_h);
            self.terminal_pane(ui, ctx, i, egui::Rect::from_min_size(pos, cell));
        }

        // Draggable divider between the two columns (double-click resets).
        let div_x = area.left() + w0 + gap * 0.5;
        let div = egui::Rect::from_min_size(
            egui::pos2(div_x - 3.0, area.top()),
            egui::vec2(6.0, area.height()),
        );
        let resp = ui.interact(div, egui::Id::new("split_divider"), Sense::drag());
        if resp.dragged() {
            self.split_ratio = (self.split_ratio + resp.drag_delta().x / (area.width() - gap))
                .clamp(0.25, 0.75);
        }
        if resp.double_clicked() {
            self.split_ratio = 0.5;
        }
        let extra = if resp.hovered() || resp.dragged() {
            theme::ACCENT()
        } else {
            theme::BORDER()
        };
        ui.painter().line_segment(
            [egui::pos2(div_x, area.top() + 6.0), egui::pos2(div_x, area.bottom() - 6.0)],
            Stroke::new(if resp.hovered() || resp.dragged() { 2.0_f32 } else { 1.0 }, extra),
        );
    }

    fn terminal_pane(&mut self, ui: &mut Ui, ctx: &egui::Context, index: usize, pane: egui::Rect) {
        let Some(Tab::Terminal(t)) = self.tabs.get_mut(index) else {
            return;
        };
        let title = t.title.clone();
        let (dot, word) = match t.status {
            TermStatus::Connected => (theme::SUCCESS(), "connected"),
            TermStatus::Connecting => (theme::WARNING(), "connecting"),
            TermStatus::Closed(_) => (theme::DANGER(), "closed"),
        };
        // The focused pane is the one with keyboard focus (falls back to the
        // selected tab), so the highlight follows clicks into terminals.
        let focused = t.is_focused(ctx) || self.active == index;
        let mut want_close = false;

        let bar = egui::Rect::from_min_size(
            pane.min,
            egui::vec2(pane.width(), 26.0),
        );
        let strip = egui::Rect::from_min_max(
            egui::pos2(pane.left() + 1.0, pane.bottom() - 18.0),
            egui::pos2(pane.right() - 1.0, pane.bottom() - 1.0),
        );
        let body = egui::Rect::from_min_max(
            egui::pos2(pane.left() + 1.0, pane.top() + 27.0),
            egui::pos2(pane.right() - 1.0, strip.top()),
        );

        let p = ui.painter();
        p.rect_filled(pane, 10.0, theme::BG_CARD());
        p.rect_stroke(
            pane,
            10.0,
            Stroke::new(
                1.0_f32,
                if focused { theme::ACCENT() } else { theme::BORDER() },
            ),
            egui::StrokeKind::Inside,
        );
        if focused {
            p.rect_filled(bar, 0.0, theme::BG_CARD_SEL());
        }
        p.circle_filled(egui::pos2(bar.left() + 14.0, bar.center().y), 3.0, dot);
        let font = egui::TextStyle::Body.resolve(ui.style());
        p.text(
            egui::pos2(bar.left() + 24.0, bar.center().y),
            egui::Align2::LEFT_CENTER,
            &title,
            font,
            theme::TEXT_DIM(),
        );

        // Bottom status strip, like a pane footer.
        p.rect_filled(strip, 0.0, theme::BG_PANEL_ALT());
        p.text(
            egui::pos2(strip.left() + 10.0, strip.center().y),
            egui::Align2::LEFT_CENTER,
            format!("{} · {}", title, word),
            egui::FontId::proportional(9.5),
            theme::TEXT_FAINT(),
        );

        // Close ×: painted + hit-tested manually so it sits cleanly at the
        // bar's right edge without disturbing layout.
        let x_rect = egui::Rect::from_min_size(
            egui::pos2(bar.right() - 26.0, bar.top() + 4.0),
            egui::vec2(22.0, 18.0),
        );
        {
            let resp = ui.interact(x_rect, egui::Id::new(("split_pane_x", index)), Sense::click());
            let (fg, bg) = if resp.hovered() {
                (theme::TEXT(), theme::BG_BTN_HOVER())
            } else {
                (theme::TEXT_FAINT(), Color32::TRANSPARENT)
            };
            if bg != Color32::TRANSPARENT {
                p.rect_filled(x_rect, 5.0, bg);
            }
            p.text(
                x_rect.center(),
                egui::Align2::CENTER_CENTER,
                "×",
                egui::FontId::proportional(11.5),
                fg,
            );
            if resp.clicked() {
                want_close = true;
            }
        }
        // Clicking anywhere else on the title bar selects the pane.
        let bar_clicked = ui
            .interact(
                egui::Rect::from_min_max(
                    egui::pos2(bar.left() + 4.0, bar.top()),
                    egui::pos2(x_rect.left(), bar.bottom()),
                ),
                egui::Id::new(("split_pane_bar", index)),
                Sense::click(),
            )
            .clicked();

        // Terminal body: pinned to the exact rect; the terminal computes its
        // grid from this rect and paints with its own painter.
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(body).id_salt(("split_body", index)),
            |ui| {
                t.ui(ui);
            },
        );

        if want_close {
            t.want_close = true;
        }
        if bar_clicked {
            self.active = index;
        }
    }

    fn tunnels_page(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        // MobaXterm-style home hero: brand, quick actions and a theme
        // picker, shown while no shells are open. Once sessions are running
        // the tunnel list takes the whole page back.
        if self.terminal_tab_count() == 0 {
            welcome_hero(self, ui, ctx);
        }
        ui.horizontal(|ui| {
            // A deliberate color bar anchoring the page title.
            let (title_bar, _) = ui.allocate_exact_size(egui::vec2(4.0, 30.0), Sense::hover());
            ui.painter().rect_filled(title_bar, 2.0, theme::ACCENT());
            ui.vertical(|ui| {
                // Reserve room for the action first: on a narrow canvas (e.g.
                // with the AI Copilot panel open) the description must wrap
                // before the button, never run underneath it.
                let reserve = 118.0;
                ui.set_max_width((ui.available_width() - reserve).max(160.0));
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.label(
                    RichText::new("Port forwarding")
                        .text_style(theme::h1())
                        .strong(),
                );
                ui.label(
                    RichText::new(
                        "Runs over a saved session or on its own SSH host. Pick the connection, choose a type, fill in the ports.",
                    )
                    .size(11.5)
                    .color(theme::TEXT_DIM()),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let new_tunnel = styled_button(
                    ui,
                    RichText::new("+  New tunnel").size(12.5),
                    true,
                    false,
                    None,
                );
                if new_tunnel.clicked() {
                    self.tunnel_editor = Some(TunnelEditor::new_standalone());
                }
            });
        });
        ui.add_space(12.0);

        // Snapshot rows, render, then apply actions (keeps borrows simple).
        struct Row {
            session_id: String,
            session_name: String,
            conn: String,
            forward: PortForward,
            phase: Option<TunnelPhase>,
            error: Option<String>,
            auth: &'static str,
            /// None: bound to a saved session. Some: standalone tunnel id.
            standalone: bool,
        }
        let rows: Vec<Row> = {
            let tunnels = &self.tunnels;
            self.standalone_tunnels
                .iter()
                .flat_map(|t| {
                    let tunnel_id = t.id.clone();
                    let label = t.label();
                    let conn = t.connection_label();
                    let auth = match t.auth_type {
                        AuthType::Password => "Password",
                        AuthType::Key => "Key",
                    };
                    t.forwards.iter().map(move |f| {
                        let key = standalone_key(&tunnel_id, &f.id);
                        Row {
                            session_id: tunnel_id.clone(),
                            session_name: label.clone(),
                            conn: conn.clone(),
                            forward: f.clone(),
                            phase: tunnels.phase(&key),
                            error: tunnels.last_error(&key),
                            auth,
                            standalone: true,
                        }
                    })
                })
                .chain(
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
                            s.forwards.iter().map(move |f| {
                                let key = forward_key(&session_id, &f.id);
                                Row {
                                    session_id: session_id.clone(),
                                    session_name: session_name.clone(),
                                    conn: conn.clone(),
                                    forward: f.clone(),
                                    phase: tunnels.phase(&key),
                                    error: tunnels.last_error(&key),
                                    auth,
                                    standalone: false,
                                }
                            })
                        }),
                )
                .collect()
        };

        if rows.is_empty() {
            let hint = if self.sessions.is_empty() {
                (
                    "No tunnels yet",
                    "Click “New tunnel”, type an SSH host and the ports to forward. A saved session is not required.",
                )
            } else {
                (
                    "No tunnels yet",
                    "Click “New tunnel” to forward a port over one of your sessions. Start and stop each tunnel any time.",
                )
            };
            empty_hint(ui, hint.0, hint.1);
            return;
        }

        let mut actions: Vec<(RowAction, String, String, bool)> = Vec::new();
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
                        row.phase,
                        row.error.as_deref(),
                        row.auth,
                    ) {
                        actions.push((
                            a,
                            row.session_id.clone(),
                            row.forward.id.clone(),
                            row.standalone,
                        ));
                    }
                }
                ui.add_space(12.0);
            });

        for (action, session_id, forward_id, standalone) in actions {
            match action {
                RowAction::Toggle => self.toggle_tunnel(&session_id, &forward_id),
                RowAction::ToggleAuto => self.toggle_tunnel_auto(&session_id, &forward_id),
                RowAction::Edit => {
                    if standalone {
                        if let Some(t) = self
                            .standalone_tunnels
                            .iter()
                            .find(|t| t.id == session_id)
                        {
                            self.tunnel_editor = Some(TunnelEditor::from_standalone(t));
                        }
                    } else if let Some(f) = self
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
                    // Standalone tunnels live in their own list; look there
                    // too so the confirm dialog has a real label and the right
                    // running key.
                    let (label, key) = if standalone {
                        let label = self
                            .standalone_tunnels
                            .iter()
                            .find(|t| t.id == session_id)
                            .and_then(|t| t.forwards.iter().find(|f| f.id == forward_id))
                            .map(forward_desc)
                            .unwrap_or_else(|| "this tunnel".to_string());
                        (label, standalone_key(&session_id, &forward_id))
                    } else {
                        let label = self
                            .sessions
                            .iter()
                            .find(|s| s.id == session_id)
                            .and_then(|s| s.forwards.iter().find(|f| f.id == forward_id))
                            .map(forward_desc)
                            .unwrap_or_else(|| "this tunnel".to_string());
                        (label, forward_key(&session_id, &forward_id))
                    };
                    self.confirm = Some(Confirm::Tunnel {
                        key,
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
            ui.set_width(580.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Session settings").size(13.0).strong());
                ui.add_space(4.0);
                ui.label(
                    RichText::new(if is_new { "new" } else { "edit" })
                        .size(10.0)
                        .color(theme::TEXT_FAINT()),
                );
            });
            ui.add_space(8.0);

            // Protocol picker: square tiles, like MobaXterm's session dialog.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if proto_tile(
                    ui,
                    tglyph::SESSION,
                    theme::SUCCESS(),
                    "SSH",
                    ed.protocol == Protocol::Ssh,
                    false,
                )
                .clicked()
                {
                    ed.protocol = Protocol::Ssh;
                    if ed.port.trim() == "3389" {
                        ed.port = "22".to_string();
                    }
                    ed.error = None;
                }
                if proto_tile(
                    ui,
                    tglyph::RAIL_FILES,
                    theme::INFO(),
                    "RDP",
                    ed.protocol == Protocol::Rdp,
                    true,
                )
                .clicked()
                {
                    ed.protocol = Protocol::Rdp;
                    if ed.port.trim() == "22" {
                        ed.port = "3389".to_string();
                    }
                    ed.error = None;
                }
            });
            ui.add_space(12.0);

            group_box(ui, "Basic SSH settings", |ui| {
                form_row(ui, "Remote host *", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut ed.host)
                            .hint_text("10.0.0.5 or server.example.com")
                            .desired_width(260.0)
                            .margin(egui::vec2(6.0, 3.0)),
                    );
                });
                ui.add_space(3.0);
                form_row(ui, "Username", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut ed.username)
                            .hint_text("root")
                            .desired_width(200.0)
                            .margin(egui::vec2(6.0, 3.0)),
                    );
                });
                ui.add_space(3.0);
                form_row(ui, "Port", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut ed.port)
                            .desired_width(56.0)
                            .margin(egui::vec2(6.0, 3.0)),
                    );
                });
                ui.add_space(3.0);
                form_row(ui, "Session name", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut ed.name)
                            .hint_text("prod web server")
                            .desired_width(260.0)
                            .margin(egui::vec2(6.0, 3.0)),
                    );
                });
                ui.add_space(3.0);
                form_row(ui, "Group", |ui| {
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
                            RichText::new(selected_text).size(12.0).color(theme::TEXT()),
                        )
                        .width(240.0)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(ed.group.is_none(), "No group")
                                .clicked()
                            {
                                ed.group = None;
                            }
                            for (id, name) in options {
                                if ui
                                    .selectable_label(
                                        ed.group.as_deref() == Some(id.as_str()),
                                        name,
                                    )
                                    .clicked()
                                {
                                    ed.group = Some(id.clone());
                                }
                            }
                        });
                });
            });

            ui.add_space(10.0);
            group_box(
                ui,
                if ed.protocol == Protocol::Rdp {
                    "RDP"
                } else {
                    "Authentication"
                },
                |ui| {
                    if ed.protocol == Protocol::Rdp {
                        ui.label(
                            RichText::new(
                                "RDP launches the Windows remote-desktop client, which opens its own login window. Port defaults to 3389.",
                            )
                            .size(10.5)
                            .color(theme::TEXT_FAINT()),
                        );
                    } else {
                        ui.horizontal(|ui| {
                            if choice_pill(ui, "Password", ed.auth_type == AuthType::Password)
                                .clicked()
                            {
                                ed.auth_type = AuthType::Password;
                                ed.error = None;
                            }
                            if choice_pill(ui, "Private key", ed.auth_type == AuthType::Key)
                                .clicked()
                            {
                                ed.auth_type = AuthType::Key;
                                ed.error = None;
                            }
                        });
                        ui.add_space(6.0);

            match ed.auth_type {
                            AuthType::Password => {
                                form_row(ui, "Password", |ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut ed.password)
                                            .password(true)
                                            .desired_width(260.0)
                                            .margin(egui::vec2(6.0, 3.0)),
                                    );
                                });
                            }
                            AuthType::Key => {
                                form_row(ui, "Private key", |ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut ed.key_path)
                                        .hint_text("C:\\Users\\me\\.ssh\\id_ed25519")
                                        .desired_width(200.0)
                                        .margin(egui::vec2(6.0, 3.0)),
                                    );
                                    ui.add_space(4.0);
                                    if ui.button("Browse...").clicked() {
                                        if let Some(p) = rfd::FileDialog::new().pick_file() {
                                            ed.key_path = p.display().to_string();
                                        }
                                    }
                                });
                                ui.add_space(3.0);
                                form_row(ui, "Passphrase", |ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut ed.key_passphrase)
                                            .password(true)
                                            .desired_width(260.0)
                                            .margin(egui::vec2(6.0, 3.0)),
                                    );
                                });
                            }
                        }
                    }
                },
            );

            if let Some(err) = &ed.error {
                ui.add_space(4.0);
                ui.label(RichText::new(err).size(11.0).color(theme::DANGER()));
            }

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                let total = 198.0;
                ui.add_space(((ui.available_width() - total) * 0.5).max(0.0));
                if dialog_button(ui, '\u{2713}', theme::SUCCESS(), "OK", true).clicked() {
                    save = true;
                }
                ui.add_space(6.0);
                if dialog_button(ui, '\u{00d7}', theme::DANGER(), "Cancel", false).clicked() {
                    keep = false;
                }
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
                    .size(16.0)
                    .strong(),
            );
            ui.label(
                RichText::new("Runs in the background while you work — over a saved session or its own SSH host.")
                    .size(10.5)
                    .color(theme::TEXT_FAINT()),
            );
            ui.add_space(12.0);

            field(ui, "Via session", |ui| {
                let selected_label = if ed.standalone {
                    "No session — enter SSH details below".to_string()
                } else {
                    session_names
                        .iter()
                        .find(|(id, _)| id == &ed.session_id)
                        .map(|(_, l)| l.clone())
                        .unwrap_or_else(|| "Choose a session…".to_string())
                };
                egui::ComboBox::from_id_salt("tunnel_session")
                    .selected_text(selected_label)
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(ed.standalone, "No session — enter SSH details below")
                            .clicked()
                        {
                            ed.standalone = true;
                            ed.session_id = STANDALONE_TUNNEL.to_string();
                        }
                        for (id, label) in &session_names {
                            if ui.selectable_label(!ed.standalone && id == &ed.session_id, label)
                                .clicked()
                            {
                                ed.standalone = false;
                                ed.session_id = id.clone();
                            }
                        }
                    });
            });

            if ed.standalone {
                field(ui, "Label (optional)", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut ed.name)
                            .hint_text("my database tunnel")
                            .desired_width(ui.available_width())
                            .margin(egui::vec2(8.0, 6.0)),
                    );
                });
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 74.0).max(120.0);
                    ui.vertical(|ui| {
                        field(ui, "SSH host", |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut ed.host)
                                    .hint_text("jam.example.com")
                                    .desired_width(w)
                                    .margin(egui::vec2(8.0, 6.0)),
                            );
                        });
                    });
                    field(ui, "Port", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut ed.ssh_port)
                                .desired_width(62.0)
                                .margin(egui::vec2(8.0, 6.0)),
                        );
                    });
                });
                ui.horizontal(|ui| {
                    let w = (ui.available_width() - 74.0).max(120.0);
                    ui.vertical(|ui| {
                        field(ui, "SSH username", |ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut ed.username)
                                    .hint_text("deploy")
                                    .desired_width(w)
                                    .margin(egui::vec2(8.0, 6.0)),
                            );
                        });
                    });
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Auth")
                                .size(11.0)
                                .color(theme::TEXT_DIM()),
                        );
                        ui.horizontal(|ui| {
                            if choice_pill(ui, "Password", ed.auth_type == AuthType::Password)
                                .clicked()
                            {
                                ed.auth_type = AuthType::Password;
                            }
                            if choice_pill(ui, "Key", ed.auth_type == AuthType::Key).clicked() {
                                ed.auth_type = AuthType::Key;
                            }
                        });
                    });
                });
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
                        field(ui, "Private key file", |ui| {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut ed.key_path)
                                        .hint_text(r"C:\Users\you\.ssh\id_ed25519")
                                        .desired_width(ui.available_width() - 70.0)
                                        .margin(egui::vec2(8.0, 6.0)),
                                );
                                if ui.button("Browse…").clicked() {
                                    if let Some(p) = rfd::FileDialog::new().pick_file() {
                                        ed.key_path = p.display().to_string();
                                    }
                                }
                            });
                        });
                    }
                }
                ui.add_space(6.0);
            }

            ui.label(
                RichText::new("Type")
                    .size(11.0)
                    .color(theme::TEXT_DIM()),
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
                .size(10.5)
                .color(theme::TEXT_FAINT()),
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
                ui.label(RichText::new(err).size(11.0).color(theme::DANGER()));
            }

            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if styled_button(
                        ui,
                        RichText::new("Save").size(12.5).strong(),
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
                    .size(16.0)
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
                .size(10.5)
                .color(theme::TEXT_FAINT()),
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
                                    .rect_filled(rect.shrink(2.0), 8.0, theme::BG_PANEL_ALT());
                                ui.painter()
                                    .rect_stroke(rect.shrink(2.0), 8.0, Stroke::new(1.0_f32, theme::BORDER_STRONG()), egui::StrokeKind::Inside);
                            }
                        }
                        if selected {
                            ui.painter()
                                .rect_stroke(rect, 9.0, Stroke::new(2.0_f32, theme::ACCENT()), egui::StrokeKind::Inside);
                        }
                        if resp.clicked() {
                            ed_ref.color = sw.map(|c| [c.r(), c.g(), c.b()]);
                        }
                        if i == 0 {
                            ui.label(
                                RichText::new("(none) = gray-free")
                                    .size(9.5)
                                    .color(theme::TEXT_FAINT()),
                            );
                        }
                    }
                });
            });
            if let Some(err) = &ed_ref.error {
                ui.label(RichText::new(err).size(11.0).color(theme::DANGER()));
            }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if styled_button(ui, RichText::new("Save").size(12.5).strong(), true, false, None)
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
            ui.label(RichText::new(title).size(15.0).strong());
            ui.add_space(4.0);
            ui.label(RichText::new(body).size(11.5).color(theme::TEXT_DIM()));
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if styled_button(
                        ui,
                        RichText::new(ok_label).size(12.5).strong(),
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
                ui.label(RichText::new("Activity").size(16.0).strong());
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
                                .size(11.0)
                                .color(theme::TEXT_DIM()),
                        );
                    }
                    for e in &entries {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            ui.label(
                                RichText::new(&e.time)
                                    .size(10.0)
                                    .color(theme::TEXT_FAINT())
                                    .monospace(),
                            );
                            let (tag, color) = match e.level {
                                LogLevel::Info => ("info", theme::INFO()),
                                LogLevel::Success => (" ok ", theme::SUCCESS()),
                                LogLevel::Error => ("err ", theme::DANGER()),
                            };
                            ui.label(RichText::new(tag).size(10.0).color(color).monospace());
                            ui.label(RichText::new(&e.session).size(10.5).strong());
                            ui.label(
                                RichText::new(&e.message)
                                    .size(11.0)
                                    .color(theme::TEXT_DIM()),
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

    /// Snippet (macro) manager: edit the list inline, persist on Save,
    /// revert on Cancel/Esc/✕.
    fn snippets_modal(&mut self, ctx: &egui::Context) {
        if !self.snippets_open {
            return;
        }
        let esc = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let mut keep = true;
        let mut save = false;
        let resp = egui::Modal::new(egui::Id::new("snippets_modal")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.label(RichText::new("Command snippets").size(16.0).strong());
            ui.label(
                RichText::new("One-click commands in the terminal quick-bar Macros menu.")
                    .size(10.5)
                    .color(theme::TEXT_FAINT()),
            );
            ui.add_space(10.0);

            egui::ScrollArea::vertical()
                .max_height(320.0)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let mut drop: Option<String> = None;
                    for s in self.snippets.iter_mut() {
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [130.0, 0.0],
                                egui::TextEdit::singleline(&mut s.name)
                                    .hint_text("Name")
                                    .margin(egui::vec2(8.0, 4.0)),
                            );
                            ui.add_sized(
                                [300.0, 0.0],
                                egui::TextEdit::singleline(&mut s.command)
                                    .hint_text("command (sent, then Enter)")
                                    .margin(egui::vec2(8.0, 4.0)),
                            );
                            if ghost_button(
                                ui,
                                RichText::new("✕").size(11.0).color(theme::DANGER()),
                                false,
                            )
                            .on_hover_text("Delete snippet")
                            .clicked()
                            {
                                drop = Some(s.id.clone());
                            }
                        });
                        ui.add_space(2.0);
                    }
                    if let Some(id) = drop {
                        self.snippets.retain(|s| s.id != id);
                    }
                    if self.snippets.is_empty() {
                        ui.label(
                            RichText::new("No snippets — add one below.")
                                .size(10.5)
                                .color(theme::TEXT_DIM()),
                        );
                    }
                });
            ui.add_space(8.0);
            if styled_button(
                ui,
                RichText::new("+  New snippet").size(12.0),
                true,
                false,
                Some(egui::vec2(ui.available_width(), 24.0)),
            )
            .clicked()
            {
                self.snippets.push(crate::snippets::Snippet::new("", ""));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(RichText::new("Save").size(12.0)))
                    .clicked()
                {
                    save = true;
                }
                if ui.button("Cancel").clicked() {
                    keep = false;
                }
            });
        });
        if resp.should_close() || esc {
            keep = false;
        }
        if keep & save {
            self.snippets.retain(|s| !s.name.trim().is_empty() || !s.command.trim().is_empty());
            match crate::snippets::save_snippets(&self.snippets) {
                Ok(_) => self.toast(ToastKind::Success, "Snippets saved"),
                Err(e) => self.toast(ToastKind::Error, format!("Could not save snippets: {e}")),
            }
            keep = false;
        }
        if !keep {
            if !save {
                // Cancel: revert in-place edits.
                self.snippets = self.snippets_backup.clone();
            }
            self.snippets_backup = Vec::new();
            self.snippets_open = false;
        }
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
                    .size(10.5)
                    .color(theme::SB_TEXT()),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(10.0);
                ui.label(
                    RichText::new(version::tag())
                        .size(10.0)
                        .color(theme::SB_TEXT_FAINT()),
                )
                .on_hover_text(version::display());
                ui.add_space(8.0);
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("Activity")
                                .size(10.5)
                                .color(theme::SB_TEXT()),
                        )
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

    // -----------------------------------------------------------------------
    // MCP modal
    // -----------------------------------------------------------------------

    fn mcp_modal(&mut self, ctx: &egui::Context) {
        if !self.mcp_modal {
            return;
        }
        let mut keep = true;
        let resp = egui::Modal::new(egui::Id::new("mcp_modal")).show(ctx, |ui| {
            ui.set_width(600.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("MCP server").size(16.0).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Close").clicked() {
                        keep = false;
                    }
                });
            });
            ui.add_space(2.0);
            ui.label(
                RichText::new(
                    "Let an MCP client (Claude Desktop, Cursor, any agent) list, create, edit, delete, start and stop saved sessions and tunnels. \
                     It listens on loopback only and every request must carry the token below.",
                )
                .size(11.0)
                .color(theme::TEXT_DIM()),
            );
            ui.add_space(8.0);

            let running = self.mcp.is_some();
            ui.horizontal(|ui| {
                let (dot, color) = if running {
                    ("● running", theme::SUCCESS())
                } else {
                    ("○ stopped", theme::TEXT_FAINT())
                };
                ui.label(RichText::new(dot).size(11.0).color(color));
                ui.add_space(6.0);
                if running {
                    if ui.button("Stop").clicked() {
                        self.mcp = None;
                        self.mcp_cfg.enabled = false;
                        if let Err(e) = self.mcp_cfg.save() {
                            self.mcp_error = Some(format!("could not write mcp.json: {e}"));
                        }
                    }
                } else if ui.button("Start").clicked() {
                    match mcp::Server::start(&self.mcp_cfg, ctx.clone()) {
                        Ok(s) => {
                            self.mcp = Some(s);
                            self.mcp_cfg.enabled = true;
                            self.mcp_started_once = true;
                            self.mcp_error = None;
                            if let Err(e) = self.mcp_cfg.save() {
                                self.mcp_error =
                                    Some(format!("running, but mcp.json failed: {e}"));
                            }
                        }
                        Err(e) => self.mcp_error = Some(e),
                    }
                }
                if ui.button("Copy client config").clicked() {
                    ctx.copy_text(client_config());
                }
                if ui.button("New token").clicked() {
                    let was_running = self.mcp.is_some();
                    self.mcp = None;
                    self.mcp_cfg.token = mcp::new_token();
                    if was_running {
                        match mcp::Server::start(&self.mcp_cfg, ctx.clone()) {
                            Ok(s) => {
                                self.mcp = Some(s);
                                self.mcp_error = None;
                            }
                            Err(e) => self.mcp_error = Some(e),
                        }
                    }
                    if let Err(e) = self.mcp_cfg.save() {
                        self.mcp_error = Some(format!("could not write mcp.json: {e}"));
                    }
                }
            });
            if let Some(err) = &self.mcp_error {
                ui.add_space(4.0);
                ui.label(RichText::new(err).size(10.5).color(theme::DANGER()));
            }

            ui.add_space(10.0);
            // Show what the running server actually uses; while stopped, show
            // what it will use on the next Start.
            let (endpoint, token) = match &self.mcp {
                Some(server) => (server.url(), server.token().to_string()),
                None => (self.mcp_cfg.url(), self.mcp_cfg.token.clone()),
            };
            egui::Grid::new("mcp_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("Endpoint")
                            .size(10.5)
                            .color(theme::TEXT_DIM()),
                    );
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(&endpoint)
                                .size(11.0)
                                .monospace()
                                .color(theme::TEXT()),
                        );
                        if ui.small_button("Copy").clicked() {
                            ctx.copy_text(endpoint.clone());
                        }
                    });
                    ui.end_row();
                    ui.label(RichText::new("Token").size(10.5).color(theme::TEXT_DIM()));
                    ui.horizontal(|ui| {
                        let shown = if self.mcp_reveal {
                            token.clone()
                        } else {
                            "•".repeat(28)
                        };
                        ui.label(
                            RichText::new(shown)
                                .size(11.0)
                                .monospace()
                                .color(theme::TEXT()),
                        );
                        if ui
                            .small_button(if self.mcp_reveal { "Hide" } else { "Show" })
                            .clicked()
                        {
                            self.mcp_reveal = !self.mcp_reveal;
                        }
                        if ui.small_button("Copy").clicked() {
                            ctx.copy_text(token.clone());
                        }
                    });
                    ui.end_row();
                });

            ui.add_space(12.0);
            ui.label(
                RichText::new("Client config (stdio shim)")
                    .size(11.0)
                    .strong(),
            );
            ui.label(
                RichText::new(
                    "Add this to Claude Desktop / Cursor's MCP config and restart the client. \
                     The shim forwards to the running app and reads the token from mcp.json.",
                )
                .size(10.5)
                .color(theme::TEXT_DIM()),
            );
            let mut snippet = client_config();
            ui.add(
                egui::TextEdit::multiline(&mut snippet)
                    .font(egui::TextStyle::Monospace)
                    .desired_rows(6)
                    .desired_width(f32::INFINITY)
                    .interactive(false),
            );
            ui.label(
                RichText::new("HTTP clients: POST JSON-RPC to the endpoint with `Authorization: Bearer <token>`.")
                    .size(10.5)
                    .color(theme::TEXT_DIM()),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!(
                    "Settings: {}",
                    mcp::config_file().display()
                ))
                .size(10.0)
                .monospace()
                .color(theme::TEXT_FAINT()),
            );
        });
        if resp.should_close() {
            keep = false;
        }
        self.mcp_modal = keep;
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
            ToastKind::Info => (theme::INFO(), "i"),
            ToastKind::Success => (theme::SUCCESS(), "✓"),
            ToastKind::Error => (theme::DANGER(), "!"),
        };
        let text = t.text.clone();
        egui::Area::new(egui::Id::new("toast"))
            .anchor(Align2::RIGHT_BOTTOM, egui::vec2(-14.0, -42.0))
            .interactable(false)
            .show(ctx, |ui| {
                Frame::NONE
                    .fill(with_alpha(theme::BG_CARD(), fade))
                    .corner_radius(CornerRadius::same(2))
                    .stroke(Stroke::new(1.0_f32, with_alpha(stroke_c, fade)))
                    .inner_margin(Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(icon)
                                    .size(11.0)
                                    .color(with_alpha(stroke_c, fade))
                                    .strong(),
                            );
                            ui.label(RichText::new(text).size(11.5).color(with_alpha(theme::TEXT(), fade)));
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
    // Flat tree row, MobaXterm's session list: no card, no shadow, just a
    // classic full-width selection highlight.
    let frame = Frame::NONE
        .fill(if selected {
            theme::BG_CARD_SEL()
        } else {
            Color32::TRANSPARENT
        })
        .corner_radius(CornerRadius::same(2))
        .inner_margin(Margin::symmetric(4, 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                // Colorful per-session tile like a host-list icon: group
                // color when grouped, brand accent otherwise, letter inside.
                let tile_color = group_dot.unwrap_or(theme::ACCENT());
                // Mini terminal icon: dark screen, group-coloured border and a
                // green prompt. Original artwork drawn with the painter (no
                // MobaXterm assets copied).
                let (tile, _) =
                    ui.allocate_exact_size(egui::vec2(16.0, 16.0), Sense::hover());
                let p = ui.painter();
                p.rect_filled(tile, 2.5, Color32::from_rgb(30, 30, 30));
                p.rect_stroke(
                    tile,
                    2.5,
                    Stroke::new(1.0_f32, tile_color),
                    egui::StrokeKind::Inside,
                );
                let c = tile.center();
                let prompt = Stroke::new(1.1_f32, Color32::from_rgb(126, 217, 87));
                p.line_segment(
                    [c + egui::vec2(-3.5, -3.0), c + egui::vec2(-1.0, -0.5)],
                    prompt,
                );
                p.line_segment(
                    [c + egui::vec2(-1.0, -0.5), c + egui::vec2(-3.5, 2.0)],
                    prompt,
                );
                p.line_segment(
                    [c + egui::vec2(0.0, 2.0), c + egui::vec2(3.5, 2.0)],
                    prompt,
                );
                // Show the whole name whenever the row has room; truncate
                // only when the icon, badge and status dot need the space.
                let chip_w = ui
                    .painter()
                    .layout_no_wrap(
                        auth_label.to_owned(),
                        egui::FontId::proportional(9.5),
                        a_fg,
                    )
                    .size()
                    .x
                    + 12.0;
                let name_w =
                    (ui.available_width() - chip_w - 6.0 - 16.0 - 8.0).max(48.0);
                ui.scope(|ui| {
                    ui.set_max_width(name_w);
                    ui.add(
                        egui::Label::new(
                            RichText::new(name).size(12.0).color(theme::TEXT()),
                        )
                        .truncate(),
                    )
                    .on_hover_text(conn);
                });
                chip(ui, auth_label, a_fg, a_bg);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if tunnel_live {
                        ui.label(RichText::new("●").size(9.0).color(theme::SUCCESS()))
                            .on_hover_text("Tunnels running");
                    } else if tab_open {
                        ui.label(RichText::new("●").size(9.0).color(theme::ACCENT()))
                            .on_hover_text("Shell open");
                    }
                });
            });
        });
    let resp = frame.response.interact(Sense::click());

    if resp.clicked() {
        action = Some(CardAction::Select);
    }
    if resp.double_clicked() {
        // Session window: lands on the server's file browser (WinSCP-style),
        // switch to Shell from the in-window toolbar.
        action = Some(CardAction::Connect);
    }
    let resp = resp.on_hover_text("Double-click to open the session · right-click for more");
    resp.context_menu(|ui| {
        if ui.button("Open shell").clicked() {
            action = Some(CardAction::OpenShell);
            ui.close();
        }
        if auth_label != "RDP" && ui.button("Browse files (SFTP)").clicked() {
            action = Some(CardAction::BrowseFiles);
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
            RichText::new("Move to group").size(10.0).color(theme::TEXT_FAINT()),
        );
        if ui.button("— No group —").clicked() {
            action = Some(CardAction::MoveToGroup(String::new()));
            ui.close();
        }
        if groups_menu.is_empty() {
            ui.label(
                RichText::new("(no groups yet — create one)").size(9.5).color(theme::TEXT_FAINT()),
            );
        } else {
            for (gid, gname) in groups_menu {
                if ui.button(gname).clicked() {
                    action = Some(CardAction::MoveToGroup(gid.clone()));
                    ui.close();
                }
            }
        }
        if ui.button("+ New group…").clicked() {
            action = Some(CardAction::NewGroup(session_id.to_string()));
            ui.close();
        }
        ui.separator();
        if ui.button("Delete").clicked() {
            action = Some(CardAction::Delete);
            ui.close();
        }
    });
    ui.add_space(2.0);
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
            // Tiny folder glyph, like MobaXterm's session tree.
            let (r, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(
                egui::Rect::from_min_max(r.min + egui::vec2(0.0, 4.0), r.max),
                1.5,
                c,
            );
            p.rect_filled(
                egui::Rect::from_min_size(r.min + egui::vec2(1.0, 2.0), egui::vec2(6.0, 4.0)),
                1.0,
                c,
            );
        }
        let caret = if expanded { "▾" } else { "▸" };
        if ghost_button(
            ui,
            RichText::new(format!("{caret} {name}"))
                .size(11.0)
                .strong()
                .color(theme::TEXT_DIM()),
            false,
        )
        .clicked()
        {
            cmd = Some(GroupCmd::Toggle);
        }
        ui.label(
            RichText::new(count.to_string())
                .size(9.5)
                .color(theme::TEXT_FAINT()),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let plus = ghost_button(ui, RichText::new("+").size(12.0).color(theme::TEXT_DIM()), false)
                .on_hover_text("New session in this group");
            if plus.clicked() {
                cmd = Some(GroupCmd::AddSession);
            }
            ui.menu_button(
                RichText::new("⋯").size(11.0).color(theme::TEXT_FAINT()),
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
        "Key" => (theme::VIOLET(), theme::VIOLET_BG()),
        "RDP" => (theme::WARNING(), theme::WARNING_BG()),
        _ => (theme::INFO(), theme::INFO_BG()),
    }
}

fn tunnel_row(
    ui: &mut Ui,
    session_name: &str,
    conn: &str,
    forward: &PortForward,
    phase: Option<TunnelPhase>,
    error: Option<&str>,
    auth_label: &str,
) -> Option<RowAction> {
    let mut action = None;
    let running = phase.is_some();
    let (dot_color, dot_tip, state_label, state_fg, state_bg) = match phase {
        Some(TunnelPhase::Starting) => (
            theme::WARNING(),
            "Connecting to the SSH host and setting up the forward…",
            "Connecting",
            theme::WARNING(),
            theme::WARNING_BG(),
        ),
        Some(TunnelPhase::Reconnecting) => (
            theme::WARNING(),
            "SSH link down — retrying automatically",
            "Retrying",
            theme::WARNING(),
            theme::WARNING_BG(),
        ),
        Some(TunnelPhase::Running) => (
            theme::SUCCESS(),
            "Tunnel is live",
            "Running",
            theme::SUCCESS(),
            theme::SUCCESS_BG(),
        ),
        None if error.is_some() => (
            theme::DANGER(),
            "The last attempt failed — the reason is shown below",
            "Failed",
            theme::DANGER(),
            theme::DANGER_BG(),
        ),
        None => (
            theme::INFO(),
            "Stopped",
            "Stopped",
            theme::TEXT_FAINT(),
            theme::BG_PANEL_ALT(),
        ),
    };
    let (a_fg, a_bg) = auth_palette(auth_label);

    let frame = Frame::NONE
        .fill(theme::BG_CARD())
        .corner_radius(CornerRadius::same(2))
        .stroke(Stroke::new(1.0_f32, theme::BORDER()))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("●").size(9.0).color(dot_color))
                    .on_hover_text(dot_tip);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    ui.label(RichText::new(forward_desc(forward)).size(12.0));
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        let (t_fg, t_bg) = fwd_palette(&forward.fwd_type);
                        chip(ui, forward.fwd_type.short(), t_fg, t_bg);
                        chip(ui, state_label, state_fg, state_bg).on_hover_text(dot_tip);
                        chip(ui, auth_label, a_fg, a_bg);
                        if forward.auto_reconnect {
                            chip(ui, "↻ auto", theme::ACCENT_TEXT(), theme::ACCENT_SOFT())
                                .on_hover_text("Auto-reconnect enabled");
                        }
                        ui.label(
                            RichText::new(format!("{session_name} · {conn}"))
                                .size(10.0)
                                .color(theme::TEXT_FAINT()),
                        );
                    });
                    // Surface failures right on the row; the full message is
                    // also in Activity. This includes per-connection forwarding
                    // errors while the listener itself is still "Running"
                    // (e.g. the server has AllowTcpForwarding no).
                    if let Some(err) = error {
                        let one_line = err.split_whitespace().collect::<Vec<_>>().join(" ");
                        let shown: String = if one_line.chars().count() > 110 {
                            format!("{}…", one_line.chars().take(110).collect::<String>())
                        } else {
                            one_line
                        };
                        let prefix = if running {
                            "Last connection failed"
                        } else {
                            "Last attempt failed"
                        };
                        ui.add(
                            egui::Label::new(
                                RichText::new(format!("{prefix}: {shown}"))
                                    .size(10.0)
                                    .color(theme::DANGER()),
                            )
                            .truncate(),
                        )
                        .on_hover_text(err);
                    }
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if tunnel_toggle_button(ui, running).clicked() {
                        action = Some(RowAction::Toggle);
                    }
                    if ghost_button(ui, RichText::new("Edit").size(10.5).color(theme::TEXT_DIM()), false)
                        .clicked()
                    {
                        action = Some(RowAction::Edit);
                    }
                    if ghost_button(ui, RichText::new("×").size(11.0).color(theme::TEXT_DIM()), true)
                        .on_hover_text("Delete tunnel")
                        .clicked()
                    {
                        action = Some(RowAction::Delete);
                    }
                    let auto = forward.auto_reconnect;
                    if soft_pill(
                        ui,
                        "↻ auto",
                        if auto { theme::ACCENT_TEXT() } else { theme::TEXT_FAINT() },
                        if auto { theme::ACCENT_SOFT() } else { theme::BG_PANEL_ALT() },
                        theme::ACCENT_SOFT_HOVER(),
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
    // Colored left status bar: a 3px state stripe inside the card edge.
    let rr = frame.response.rect;
    let bar = egui::Rect::from_min_size(
        egui::pos2(rr.left() + 2.0, rr.top() + 4.0),
        egui::vec2(3.0, rr.height() - 8.0),
    );
    ui.painter().rect_filled(bar, 1.6, dot_color);
    ui.add_space(6.0);
    action
}

/// Compact play/stop control: green play when stopped, red stop while a
/// tunnel exists (connecting, running or retrying), so Stop is always one
/// click away even if the SSH connect is hanging.
fn tunnel_toggle_button(ui: &mut Ui, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(30.0, 22.0), Sense::click());
    let (fg, bg, hover) = if active {
        (theme::DANGER(), theme::DANGER_BG(), Color32::from_rgb(248, 226, 226))
    } else {
        (theme::SUCCESS(), theme::SUCCESS_BG(), Color32::from_rgb(223, 241, 232))
    };
    let fill = if resp.hovered() { hover } else { bg };
    ui.painter().rect(
        rect,
        3.0,
        fill,
        Stroke::new(1.0_f32, fg.gamma_multiply(0.45)),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        Align2::CENTER_CENTER,
        if active { tglyph::STOP } else { tglyph::PLAY },
        theme::icon(14.0),
        fg,
    );
    resp.on_hover_text(if active { "Stop tunnel" } else { "Start tunnel" })
}

/// Terminal quick-bar actions surfaced to the app for toasts/flags.
#[derive(Clone, Copy, PartialEq)]
enum QuickAction {
    None,
    Copied,
    ClipboardEmpty,
    Cleared,
    Reconnect,
    ToggleFullscreen,
    Detach,
}

/// MobaXterm-style session toolbar above the shell: copy/paste/clear/
/// reconnect plus a Macros menu that fires saved snippets into the shell.
/// Drawn by the app (not the terminal) so it can read the snippet list and
/// open the manager modal; it acts on the active session directly.
fn terminal_quick_bar(
    ui: &mut Ui,
    term: &mut TerminalSession,
    snippets: &[crate::snippets::Snippet],
    want_manage: &mut bool,
    fullscreen: bool,
) -> QuickAction {
    let avail = ui.available_rect_before_wrap();
    let bar = egui::Rect::from_min_size(avail.min, egui::vec2(avail.width(), 30.0));
    ui.allocate_rect(bar, egui::Sense::hover());
    let painter = ui.painter_at(bar);
    painter.rect_filled(bar, 0.0, theme::BG_PANEL());
    painter.line_segment(
        [
            egui::pos2(bar.left(), bar.bottom() - 0.5),
            egui::pos2(bar.right(), bar.bottom() - 0.5),
        ],
        Stroke::new(0.5_f32, theme::BORDER()),
    );

    let mut action = QuickAction::None;
    ui.scope_builder(
        egui::UiBuilder::new().max_rect(bar).id_salt("terminal_quick_bar"),
        |ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.add_space(4.0);
            // MobaXterm-style icon strip: proper glyphs, tooltips on hover.
            if ghost_button(ui, RichText::new(tglyph::COPY).font(theme::icon(15.0)), false)
                .on_hover_text("Copy screen to clipboard")
                .clicked()
            {
                let text = term.visible_text();
                if text.trim().is_empty() {
                    action = QuickAction::ClipboardEmpty;
                } else {
                    ui.ctx().copy_text(text);
                    action = QuickAction::Copied;
                }
            }
            if ghost_button(ui, RichText::new(tglyph::PASTE).font(theme::icon(15.0)), false)
                .on_hover_text("Paste from clipboard")
                .clicked()
            {
                let text = arboard::Clipboard::new()
                    .and_then(|mut c| c.get_text())
                    .unwrap_or_default();
                if text.trim().is_empty() {
                    action = QuickAction::ClipboardEmpty;
                } else {
                    term.paste_text(&text);
                }
            }
            if ghost_button(ui, RichText::new(tglyph::ERASER).font(theme::icon(15.0)), false)
                .on_hover_text("Clear screen")
                .clicked()
            {
                term.clear_screen();
                action = QuickAction::Cleared;
            }
            if ghost_button(ui, RichText::new(tglyph::RELOAD).font(theme::icon(15.0)), false)
                .on_hover_text("Reconnect")
                .clicked()
            {
                term.reconnect();
                action = QuickAction::Reconnect;
            }
        });

        // Right edge: full-screen toggle + macros menu — one click fires a
        // snippet into the shell.
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(6.0);
            ui.menu_button(
                RichText::new("Macros").size(10.5).color(theme::TEXT_DIM()),
                |menu| {
                    if snippets.is_empty() {
                        menu.add_space(4.0);
                        menu.label(
                            RichText::new("No snippets yet.")
                                .size(10.5)
                                .color(theme::TEXT_FAINT()),
                        );
                    }
                    for s in snippets {
                        let label = if s.command.contains('\n') {
                            format!("{}  ▤", s.name)
                        } else {
                            s.name.clone()
                        };
                        if menu
                            .button(RichText::new(label).size(11.0))
                            .clicked()
                        {
                            let mut bytes = s.command.as_bytes().to_vec();
                            if !s.command.ends_with('\n') {
                                bytes.push(b'\r');
                            }
                            term.send_bytes(bytes);
                            menu.close();
                        }
                    }
                    menu.separator();
                    if menu
                        .button(RichText::new("Manage snippets…").size(11.0))
                        .clicked()
                    {
                        *want_manage = true;
                    }
                },
            );
            // View controls: one boxed, segmented group with proper Tabler
            // glyphs; the left segment toggles full screen, the right pops the
            // terminal into its own OS window.
            ui.add_space(2.0);
            let (toggle_fs, detach) = view_control_group(ui, fullscreen);
            if toggle_fs {
                action = QuickAction::ToggleFullscreen;
            }
            if detach {
                action = QuickAction::Detach;
            }
        });
    });
    action
}

/// Trimmed argument, or the fallback for omitted/blank values.
fn non_empty(value: Option<&str>, fallback: &str) -> String {
    match value.map(str::trim) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => fallback.to_string(),
    }
}

fn forward_kind(kind: Option<&str>) -> ForwardType {
    match kind.map(|k| k.trim().to_ascii_lowercase()).as_deref() {
        Some("remote") => ForwardType::Remote,
        Some("dynamic") => ForwardType::Dynamic,
        _ => ForwardType::Local,
    }
}

/// The stdio shim is installed next to the app (see Cargo.toml's second bin).
fn mcp_shim_path() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default()
        .join("culvert-mcp.exe")
}

fn client_config() -> String {
    let path = mcp_shim_path().display().to_string().replace('\\', "\\\\");
    format!(
        "{{\n  \"mcpServers\": {{\n    \"culvert\": {{ \"command\": \"{path}\" }}\n  }}\n}}"
    )
}

/// A single bordered control holding the full-screen and pop-out segments.
/// Proper Tabler glyphs in a shared box make the pair read as one view menu
/// instead of two unrelated, hand-drawn icons.
fn view_control_group(ui: &mut Ui, fullscreen: bool) -> (bool, bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(60.0, 22.0), Sense::hover());
    let mid = rect.center().x;
    let fs_rect = egui::Rect::from_min_max(rect.min, egui::pos2(mid, rect.max.y));
    let pop_rect = egui::Rect::from_min_max(egui::pos2(mid, rect.min.y), rect.max);
    let fs = ui.interact(fs_rect, ui.id().with("term_fs_seg"), Sense::click());
    let pop = ui.interact(pop_rect, ui.id().with("term_pop_seg"), Sense::click());

    let p = ui.painter();
    p.rect(
        rect,
        4.0,
        theme::BG_CARD(),
        Stroke::new(1.0_f32, theme::BORDER()),
        egui::StrokeKind::Inside,
    );
    if fullscreen || fs.hovered() {
        p.rect_filled(fs_rect.shrink(1.5), 3.0, theme::ACCENT_SOFT());
    }
    if pop.hovered() {
        p.rect_filled(pop_rect.shrink(1.5), 3.0, theme::ACCENT_SOFT());
    }
    p.line_segment(
        [
            egui::pos2(mid, rect.top() + 4.0),
            egui::pos2(mid, rect.bottom() - 4.0),
        ],
        Stroke::new(1.0_f32, theme::BORDER()),
    );
    p.text(
        fs_rect.center(),
        Align2::CENTER_CENTER,
        if fullscreen {
            tglyph::COLLAPSE
        } else {
            tglyph::EXPAND
        },
        theme::icon(14.0),
        if fullscreen || fs.hovered() {
            theme::ACCENT_TEXT()
        } else {
            theme::TEXT_DIM()
        },
    );
    p.text(
        pop_rect.center(),
        Align2::CENTER_CENTER,
        tglyph::POPOUT,
        theme::icon(14.0),
        if pop.hovered() {
            theme::ACCENT_TEXT()
        } else {
            theme::TEXT_DIM()
        },
    );

    let fs = fs.on_hover_text(if fullscreen {
        "Leave full screen (F11)"
    } else {
        "Full screen terminal (F11)"
    });
    let pop = pop.on_hover_text("Pop this terminal out into its own window");
    (fs.clicked(), pop.clicked())
}

/// Terminal-style tab chip; right-click opens the Chrome-style tab menu.
/// Returns (clicked-activate, clicked-close, context-menu command).

/// Flat MobaXterm-style toolbar tile: colored Tabler glyph over a small
/// label, no background fill — a soft rounded highlight appears on hover,
/// accent wash when active. Professional, quiet, and light on the eyes.
fn tool_button(
    ui: &mut Ui,
    glyph: char,
    tint: Color32,
    label: &str,
    active: bool,
    tip: &str,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(54.0, 52.0), Sense::click());
    let p = ui.painter();
    // MobaXterm toolbar tiles are borderless until hovered; the active tool
    // gets a light-blue wash and a saturated strip on its bottom edge.
    if active || resp.hovered() {
        p.rect_filled(rect, 0.0, theme::ACCENT_SOFT());
        p.rect_stroke(
            rect,
            0.0,
            Stroke::new(1.0_f32, theme::ACCENT()),
            egui::StrokeKind::Inside,
        );
    }
    if active {
        p.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left(), rect.bottom() - 3.0),
                rect.max,
            ),
            0.0,
            theme::ACCENT(),
        );
    }
    p.text(
        egui::pos2(rect.center().x, rect.top() + 20.0),
        egui::Align2::CENTER_CENTER,
        glyph,
        theme::icon(24.0),
        tint,
    );
    p.text(
        egui::pos2(rect.center().x, rect.bottom() - 10.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.0),
        if active { theme::ACCENT_TEXT() } else { theme::TEXT() },
    );
    resp.on_hover_text(tip)
}

/// Narrow far-right rail tile (26x26): flat until hovered, like the crop of
/// icons down MobaXterm's right edge.
fn rail_button(ui: &mut Ui, glyph: char, tint: Color32, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 0.0, theme::ACCENT_SOFT());
        p.rect_stroke(
            rect,
            0.0,
            Stroke::new(1.0_f32, theme::ACCENT()),
            egui::StrokeKind::Inside,
        );
    }
    p.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        glyph,
        theme::icon(15.0),
        tint,
    );
    resp.on_hover_text(tip)
}

/// Big welcome-screen action button: Tabler glyph in its color + label.
fn hero_button(
    ui: &mut Ui,
    glyph: char,
    tint: Color32,
    label: &str,
    filled: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(160.0, 34.0), Sense::click());
    let p = ui.painter();
    if filled {
        p.rect_filled(rect, 8.0, theme::ACCENT());
        let w = egui::FontId::proportional(12.5);
        p.text(
            egui::pos2(rect.left() + 24.0, rect.center().y),
            egui::Align2::CENTER_CENTER,
            glyph,
            theme::icon(14.0),
            Color32::WHITE,
        );
        p.text(
            egui::pos2(rect.left() + 42.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            w,
            Color32::WHITE,
        );
    } else {
        p.rect_filled(rect, 8.0, theme::BG_BTN());
        p.rect_stroke(rect, 8.0, Stroke::new(1.0_f32, theme::BORDER_STRONG()), egui::StrokeKind::Inside);
        p.text(
            egui::pos2(rect.left() + 24.0, rect.center().y),
            egui::Align2::CENTER_CENTER,
            glyph,
            theme::icon(14.0),
            tint,
        );
        p.text(
            egui::pos2(rect.left() + 42.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(12.5),
            theme::TEXT(),
        );
    }
    resp
}

/// Centered home hero: brand mark, version, quick-action buttons and a
/// Light/Dark theme picker with live mini previews.
fn welcome_hero(
    app: &mut TunnelApp,
    ui: &mut Ui,
    ctx: &egui::Context,
) {
    let mark = app
        .mark_texture
        .get_or_insert_with(|| theme::brand_texture(ctx, 128))
        .clone();
    ui.vertical_centered(|ui| {
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.add_space((ui.available_width() - 210.0) / 2.0);
            paint_mark(ui, &mark, 46.0);
            ui.add_space(12.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(RichText::new("Culvert").size(30.0).strong());
                ui.label(
                    RichText::new(format!("{} · SSH client · AI Copilot", version::tag()))
                        .size(11.0)
                        .color(theme::TEXT_FAINT()),
                );
            });
        });
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            ui.add_space((ui.available_width() - 330.0) / 2.0);
            if hero_button(ui, tglyph::SESSION, theme::SUCCESS(), "New session", true).clicked() {
                app.session_editor = Some(SessionEditor::new());
            }
            ui.add_space(10.0);
            if hero_button(ui, tglyph::TUNNEL, theme::INFO(), "New tunnel", false).clicked() {
                app.tunnel_editor = Some(TunnelEditor::new_standalone());
            }
        });
        ui.add_space(16.0);

        // Theme picker: two mini previews of the shell chrome.
        ui.label(
            RichText::new("Select your favorite theme")
                .size(11.0)
                .color(theme::TEXT_DIM()),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space((ui.available_width() - 470.0) / 2.0);
            let (light_active, dark_active) =
                (!theme::is_dark(), theme::is_dark());
            if theme_card(ui, "Light", false, light_active).clicked() {
                if theme::is_dark() {
                    theme::set_dark(false);
                    theme::apply(ctx);
                }
            }
            ui.add_space(14.0);
            if theme_card(ui, "Dark", true, dark_active).clicked() {
                if !theme::is_dark() {
                    theme::set_dark(true);
                    theme::apply(ctx);
                }
            }
        });
        ui.add_space(10.0);
    });
}

/// Rounded theme preview card: a small mock of the shell chrome in the
/// palette it selects, with the palette name beneath.
fn theme_card(ui: &mut Ui, name: &str, dark: bool, active: bool) -> egui::Response {
    let (bg, panel, dot, fg) = if dark {
        (
            Color32::from_rgb(13, 17, 23),
            Color32::from_rgb(22, 27, 34),
            Color32::from_rgb(63, 185, 80),
            Color32::from_rgb(110, 118, 129),
        )
    } else {
        (
            Color32::from_rgb(255, 255, 255),
            Color32::from_rgb(246, 248, 250),
            Color32::from_rgb(26, 127, 55),
            Color32::from_rgb(140, 150, 160),
        )
    };
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(228.0, 108.0), Sense::click());
    let p = ui.painter();
    let stroke = if active {
        Stroke::new(2.0_f32, theme::ACCENT())
    } else {
        Stroke::new(1.0_f32, theme::BORDER())
    };
    p.rect_filled(rect, 2.0, bg);
    p.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
    // Sidebar hint
    p.rect_filled(
        egui::Rect::from_min_size(rect.min, egui::vec2(46.0, rect.height())),
        0.0,
        panel,
    );
    // Title-bar dots + a prompt line
    let cy = rect.top() + 12.0;
    for (i, c) in [(0.0, theme::DANGER()), (10.0, theme::WARNING()), (20.0, theme::SUCCESS())] {
        p.circle_filled(egui::pos2(rect.left() + 56.0 + i, cy), 3.0, c);
    }
    p.rect_filled(
        egui::Rect::from_min_size(egui::pos2(rect.left() + 12.0, rect.top() + 42.0), egui::vec2(64.0, 5.0)),
        2.5,
        fg,
    );
    p.rect_filled(
        egui::Rect::from_min_size(
            egui::pos2(rect.left() + 58.0, rect.top() + 54.0),
            egui::vec2(56.0, 5.0),
        ),
        2.5,
        dot,
    );
    p.rect_filled(
        egui::Rect::from_min_size(
            egui::pos2(rect.left() + 12.0, rect.top() + 66.0),
            egui::vec2(90.0, 5.0),
        ),
        2.5,
        fg.gamma_multiply(0.7),
    );
    // Name below.
    let label = egui::Rect::from_min_size(
        egui::pos2(rect.left(), rect.bottom()),
        egui::vec2(rect.width(), 20.0),
    );
    p.text(label.center(), egui::Align2::CENTER_CENTER, name, egui::FontId::proportional(11.0), theme::TEXT());
    resp
}

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
    // MobaXterm-style tab: a rectangular page tab on the strip. The active
    // tab uses the work-area surface with a saturated strip along its top
    // edge; inactive tabs sit on the chrome colour.
    let tab = Frame::NONE
        .fill(if active {
            theme::BG_WINDOW()
        } else {
            theme::BG_PANEL()
        })
        .corner_radius(CornerRadius::same(2))
        .stroke(Stroke::new(1.0_f32, theme::BORDER()))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if let Some(c) = dot {
                    ui.label(RichText::new("●").size(9.0).color(c));
                }
                let mut label = RichText::new(title).size(12.0);
                if active {
                    label = label.strong().color(theme::TEXT());
                } else {
                    label = label.color(theme::TEXT_DIM());
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
                                RichText::new("×").size(9.5).color(if active {
                                    theme::TEXT_DIM()
                                } else {
                                    theme::TEXT_FAINT()
                                }),
                            )
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::NONE)
                            .corner_radius(CornerRadius::same(2)),
                        )
                        .on_hover_text("Close tab");
                    if x.clicked() {
                        closed = true;
                    }
                }
            });
        });

    // Make the whole tab a click target: its padding, dot and label. Only
    // the text used to react, which made switching tabs feel broken when a
    // click landed a pixel or two off (e.g. in a maximized window).
    if tab.response.interact(Sense::click()).clicked() && !closed {
        clicked = true;
    }

    if active {
        // Saturated top edge marks the active page, MobaXterm style.
        let r = tab.response.rect;
        ui.painter().rect_filled(
            egui::Rect::from_min_size(r.min, egui::vec2(r.width(), 2.0)),
            0.0,
            theme::ACCENT(),
        );
    }

    (clicked, closed, menu)
}

fn chip(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) -> egui::Response {
    Frame::NONE
        .fill(bg)
        .corner_radius(CornerRadius::same(2))
        .inner_margin(Margin::symmetric(6, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(9.5).color(fg).strong());
        })
        .response
}

fn choice_pill(ui: &mut Ui, label: &str, active: bool) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(label).size(11.5))
            .fill(if active {
                theme::ACCENT_SOFT()
            } else {
                theme::BG_BTN()
            })
            .stroke(Stroke::new(
                1.0_f32,
                if active {
                    theme::ACCENT()
                } else {
                    theme::BORDER()
                },
            ))
            .corner_radius(CornerRadius::same(2)),
    )
}

/// Protocol tile in the session dialog: square, icon over label, with the
/// active entry washed light blue and underlined in the accent color.
fn proto_tile(
    ui: &mut Ui,
    glyph: char,
    tint: Color32,
    label: &str,
    active: bool,
    monitor: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(64.0, 58.0), Sense::click());
    let p = ui.painter();
    p.rect_filled(
        rect,
        0.0,
        if active {
            theme::ACCENT_SOFT()
        } else {
            theme::BG_PANEL()
        },
    );
    p.rect_stroke(
        rect,
        0.0,
        Stroke::new(
            1.0_f32,
            if active || resp.hovered() {
                theme::ACCENT()
            } else {
                theme::BORDER_STRONG()
            },
        ),
        egui::StrokeKind::Inside,
    );
    if active {
        p.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left(), rect.bottom() - 3.0),
                rect.max,
            ),
            0.0,
            theme::ACCENT(),
        );
    }
    if monitor {
        // Original little monitor glyph for RDP.
        let m = egui::Rect::from_center_size(
            egui::pos2(rect.center().x, rect.top() + 20.0),
            egui::vec2(26.0, 18.0),
        );
        p.rect_filled(m, 1.0, theme::BG_INPUT());
        p.rect_stroke(
            m,
            1.0,
            Stroke::new(1.5_f32, tint),
            egui::StrokeKind::Inside,
        );
        p.line_segment(
            [
                egui::pos2(m.center().x, m.bottom()),
                egui::pos2(m.center().x, m.bottom() + 3.0),
            ],
            Stroke::new(1.5_f32, tint),
        );
        p.line_segment(
            [
                egui::pos2(m.center().x - 5.0, m.bottom() + 3.0),
                egui::pos2(m.center().x + 5.0, m.bottom() + 3.0),
            ],
            Stroke::new(1.5_f32, tint),
        );
    } else {
        p.text(
            egui::pos2(rect.center().x, rect.top() + 22.0),
            egui::Align2::CENTER_CENTER,
            glyph,
            theme::icon(24.0),
            tint,
        );
    }
    p.text(
        egui::pos2(rect.center().x, rect.bottom() - 11.0),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(11.0),
        theme::TEXT(),
    );
    resp
}

/// Classic Win32 group box: a 1px rectangle with the caption sitting on the
/// top border.
fn group_box(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    let bg = theme::BG_PANEL();
    let resp = Frame::NONE
        .stroke(Stroke::new(1.0_f32, theme::BORDER_STRONG()))
        .inner_margin(Margin {
            left: 10,
            right: 10,
            top: 14,
            bottom: 10,
        })
        .show(ui, add);
    let rect = resp.response.rect;
    let galley = ui.painter().layout_no_wrap(
        title.to_owned(),
        egui::FontId::proportional(11.0),
        theme::TEXT_DIM(),
    );
    let pos = egui::pos2(rect.left() + 9.0, rect.top() - galley.size().y * 0.5);
    ui.painter().rect_filled(
        egui::Rect::from_min_size(
            pos - egui::vec2(3.0, 1.0),
            galley.size() + egui::vec2(6.0, 2.0),
        ),
        0.0,
        bg,
    );
    ui.painter().galley(pos, galley, theme::TEXT_DIM());
    ui.add_space(8.0);
}

/// Form row: right-aligned label column, then the control(s).
fn form_row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(92.0, 20.0),
            Layout::right_to_left(Align::Center),
            |ui| {
                ui.label(RichText::new(label).size(11.0).color(theme::TEXT_DIM()));
            },
        );
        ui.add_space(6.0);
        add(ui);
    });
}

/// Centered classic dialog button: colored glyph + black label on a gray face
/// with a 1px edge.
fn dialog_button(
    ui: &mut Ui,
    glyph: char,
    tint: Color32,
    label: &str,
    primary: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(96.0, 26.0), Sense::click());
    let p = ui.painter();
    let hovered = resp.hovered();
    p.rect_filled(
        rect,
        0.0,
        if hovered {
            theme::ACCENT_SOFT()
        } else {
            theme::BG_BTN()
        },
    );
    p.rect_stroke(
        rect,
        0.0,
        Stroke::new(
            1.0_f32,
            if hovered || primary {
                theme::ACCENT()
            } else {
                theme::BORDER_STRONG()
            },
        ),
        egui::StrokeKind::Inside,
    );
    p.text(
        egui::pos2(rect.left() + 22.0, rect.center().y),
        egui::Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(15.0),
        tint,
    );
    p.text(
        egui::pos2(rect.center().x + 10.0, rect.center().y),
        egui::Align2::CENTER_CENTER,
        label,
        egui::FontId::proportional(12.0),
        theme::TEXT(),
    );
    resp
}

fn field(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.label(RichText::new(label).size(11.0).color(theme::TEXT_DIM()));
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
            (theme::DANGER_BTN(), theme::DANGER_BTN_HOVER())
        } else {
            (theme::ACCENT(), theme::ACCENT_HOVER())
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
        let button = egui::Button::new(text).corner_radius(CornerRadius::same(2));
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
            egui::Button::new(RichText::new(label).size(10.5).strong().color(fg))
                .corner_radius(CornerRadius::same(2)),
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
            theme::DANGER_BG()
        } else {
            theme::BG_BTN_HOVER()
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
                .corner_radius(CornerRadius::same(2)),
        )
    })
    .inner
}

/// Sidebar tool chip: compact painter-drawn pill with an active state and
/// an optional status dot (MobaXterm's left-tool tabs).
/// Square icon tile for the collapsed sidebar rail; Tabler glyphs, palette-
/// aware so tiles stay legible in both themes.
fn rail_tile(ui: &mut Ui, glyph: char, active: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(40.0, 40.0), Sense::click());
    let p = ui.painter();
    let (fill, stroke) = if active {
        (
            theme::ACCENT_SOFT(),
            Stroke::new(1.0_f32, theme::ACCENT()),
        )
    } else if resp.hovered() {
        (
            theme::BG_BTN_HOVER(),
            Stroke::new(1.0_f32, theme::BORDER()),
        )
    } else {
        (Color32::TRANSPARENT, Stroke::NONE)
    };
    p.rect_filled(rect, 2.0, fill);
    p.rect_stroke(rect, 2.0, stroke, egui::StrokeKind::Inside);
    let ic = if active { theme::ACCENT() } else { theme::TEXT_DIM() };
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        glyph,
        theme::icon(17.0),
        ic,
    );
    resp
}

fn empty_hint(ui: &mut Ui, title: &str, body: &str) {
    Frame::NONE
        .fill(theme::BG_PANEL_ALT())
        .corner_radius(CornerRadius::same(2))
        .stroke(Stroke::new(1.0_f32, theme::BORDER()))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(title).size(13.0).strong());
                ui.add_space(2.0);
                ui.label(
                    RichText::new(body)
                        .size(11.0)
                        .color(theme::TEXT_DIM()),
                );
            });
        });
}

/// Window controls for the custom title bar.
#[derive(Clone, Copy)]
enum WinBtn {
    Min,
    Max,
    Restore,
    Close,
}

/// Paints one title-bar window control (minimize / maximize / restore / close)
/// and returns its click response. Everything is drawn with the painter, so no
/// icon-font codepoints are needed for the window chrome.
fn window_button(ui: &mut Ui, btn: WinBtn) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(42.0, 28.0), Sense::click());
    let hovered = resp.hovered();
    let is_close = matches!(btn, WinBtn::Close);
    if hovered {
        // Close gets the conventional red wash; the rest a neutral one.
        let fill = if is_close {
            theme::DANGER()
        } else {
            theme::BG_BTN_HOVER()
        };
        ui.painter().rect_filled(rect, 0.0, fill);
    }
    let col = if hovered && is_close {
        Color32::WHITE
    } else {
        theme::TEXT_DIM()
    };
    let stroke = Stroke::new(1.0_f32, col);
    let c = rect.center();
    let p = ui.painter();
    match btn {
        WinBtn::Min => {
            p.line_segment(
                [egui::pos2(c.x - 5.0, c.y), egui::pos2(c.x + 5.0, c.y)],
                stroke,
            );
        }
        WinBtn::Max => {
            p.rect_stroke(
                egui::Rect::from_center_size(c, egui::vec2(10.0, 10.0)),
                0.0,
                stroke,
                egui::StrokeKind::Inside,
            );
        }
        WinBtn::Restore => {
            let back = egui::Rect::from_center_size(
                egui::pos2(c.x - 1.5, c.y - 1.5),
                egui::vec2(9.0, 9.0),
            );
            let front = egui::Rect::from_center_size(
                egui::pos2(c.x + 1.5, c.y + 1.5),
                egui::vec2(9.0, 9.0),
            );
            p.rect_stroke(back, 0.0, stroke, egui::StrokeKind::Inside);
            p.rect_stroke(front, 0.0, stroke, egui::StrokeKind::Inside);
        }
        WinBtn::Close => {
            p.line_segment(
                [
                    egui::pos2(c.x - 5.0, c.y - 5.0),
                    egui::pos2(c.x + 5.0, c.y + 5.0),
                ],
                stroke,
            );
            p.line_segment(
                [
                    egui::pos2(c.x + 5.0, c.y - 5.0),
                    egui::pos2(c.x - 5.0, c.y + 5.0),
                ],
                stroke,
            );
        }
    }
    resp
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

/// Display name for a session: its label, falling back to user@host:port.
fn session_display_name(s: &SshSession) -> String {
    if s.name.trim().is_empty() {
        s.connection_label()
    } else {
        s.name.trim().to_string()
    }
}

fn fwd_palette(t: &ForwardType) -> (Color32, Color32) {
    match t {
        ForwardType::Local => (theme::INFO(), theme::INFO_BG()),
        ForwardType::Remote => (theme::VIOLET(), theme::VIOLET_BG()),
        ForwardType::Dynamic => (theme::SUCCESS(), theme::SUCCESS_BG()),
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
        let mut app = TunnelApp::new_inner(rt.handle().clone(), logs, None);
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

        // Session tab: spawns against a closed port, fails fast, renders
        // both the connecting and disconnected overlays.
        app.open_session(&ctx, "demo-session");
        assert!(matches!(app.tabs.last(), Some(Tab::Terminal(_))));
        let _ = run_frames(&mut app, &ctx, egui::vec2(1024.0, 700.0), 6) > 50;
        let _ = rt;
        assert!(app.tabs.len() >= 2);
        // A second open on the same session focuses the existing tab instead
        // of spawning a duplicate (one window per server).
        app.open_session(&ctx, "demo-session");
        assert_eq!(app.tabs.len(), 2);
        assert!(matches!(app.tabs.last(), Some(Tab::Terminal(_))));
    }

    /// Regression: "New tunnel" must be savable with zero saved sessions —
    /// the editor carries the standalone sentinel, and the save path keys
    /// off the flag (not an empty session id).
    #[test]
    fn standalone_tunnel_editor_needs_no_session() {
        let ed = TunnelEditor::new_standalone();
        assert!(ed.standalone);
        assert_eq!(ed.session_id, STANDALONE_TUNNEL);
        assert!(ed.forward_id.is_none());
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

    fn sample_term_run(command: &str) -> AiTermRun {
        AiTermRun {
            id: 7,
            session_id: "demo-session".into(),
            label: "Demo server".into(),
            command: command.into(),
            nonce: "abc12345".into(),
            opened: false,
            sent: false,
            wait_for_exit: true,
            sudo_fed: 0,
            sudo_prompt_at: None,
            armed_at: std::time::Instant::now(),
            sent_at: std::time::Instant::now(),
        }
    }

    /// The typed line must not contain the printed markers verbatim — the
    /// shell echoes what we type, and a literal match would look like "done"
    /// before the command even ran.
    #[test]
    fn terminal_run_line_quotes_command_and_hides_markers() {
        let run = sample_term_run("echo 'hi there' && df -h");
        let line = run.line();
        assert!(!line.contains(&run.start_tag()));
        assert!(!line.contains(&run.exit_tag()));
        // The command survives quoting: a quote becomes '\'' inside sh -c.
        assert!(line.contains(r#"sh -c 'echo '\''hi there'\'' && df -h'"#));
        assert!(line.ends_with('\n'));

        // Newlines stay inside the single-quoted sh -c string.
        let multi = sample_term_run("cd /tmp\nls -la");
        assert!(multi.line().contains("'cd /tmp\nls -la'"));
    }

    #[test]
    fn terminal_run_reads_exit_status_and_output() {
        let run = sample_term_run("df -h");
        let screen = format!(
            "user@host:~$ printf ...\n{}\nFilesystem      Size  Used\n/dev/sda1        40G   12G\n{}0>>>\nuser@host:~$ ",
            run.start_tag(),
            run.exit_tag()
        );
        let out = ai_term_result(&screen, &run).expect("marker present");
        assert!(out.starts_with("[exit 0]"), "{out}");
        assert!(out.contains("/dev/sda1"), "{out}");
        assert!(!out.contains("printf"), "{out}");

        // Non-zero status is reported as-is.
        let failed = format!("{}\nsudo: a password is required\n{}1>>>", run.start_tag(), run.exit_tag());
        let out = ai_term_result(&failed, &run).unwrap();
        assert!(out.starts_with("[exit 1]"));
        assert!(out.contains("password is required"));

        // Still running: no marker on screen yet.
        assert!(ai_term_result("user@host:~$ df -h\n", &run).is_none());
    }

    /// A package manager run without root fails with a permission error; the
    /// hint says what to do instead of letting the model call it "installed".
    #[test]
    fn root_refusal_gets_a_sudo_hint() {
        let refused = "ERROR: Unable to open log: Permission denied";
        let hinted = crate::ai::tools::needs_root_hint(refused, "apk add ncdu");
        assert!(hinted.contains("prefixed with `sudo`"), "{hinted}");
        // Already running under sudo: nothing to add.
        assert_eq!(
            crate::ai::tools::needs_root_hint(refused, "sudo apk add ncdu"),
            refused
        );
        // Unrelated failures are untouched.
        assert_eq!(
            crate::ai::tools::needs_root_hint("cat: nope: No such file", "cat nope"),
            "cat: nope: No such file"
        );

        let run = sample_term_run("apk add ncdu");
        let screen = format!(
            "{}\nERROR: Unable to open log: Permission denied\n{}99>>>\nuser@host:~$ ",
            run.start_tag(),
            run.exit_tag()
        );
        let out = ai_term_result(&screen, &run).expect("marker present");
        assert!(out.starts_with("[exit 99]"), "{out}");
        assert!(out.contains("prefixed with `sudo`"), "{out}");
    }

    /// The saved password answers `sudo`'s prompt while a terminal run waits,
    /// and the prompt line never reaches the model.
    #[test]
    fn sudo_prompt_is_detected_and_hidden_from_the_output() {
        assert!(at_sudo_prompt(
            "some output\n[sudo] password for linuxio: "
        ));
        assert!(!at_sudo_prompt("[sudo] password for linuxio: \n$ "));
        assert!(!at_sudo_prompt("this log line mentions [sudo] password for bob"));
        assert!(!at_sudo_prompt(""));

        let run = sample_term_run("sudo apk add htop");
        let screen = format!(
            "{}\n[sudo] password for linuxio:\nOK: 45 MiB\n{}0>>>\nuser@host:~$ ",
            run.start_tag(),
            run.exit_tag()
        );
        let out = ai_term_result(&screen, &run).expect("marker present");
        assert!(!out.contains("password for"), "{out}");
        assert!(out.contains("OK: 45 MiB"), "{out}");
    }

    /// A password that reached the screen (echo race with sudo) must not travel
    /// on to the model, the card or the audit log.
    #[test]
    fn echoed_password_is_scrubbed_from_the_result() {
        let run = sample_term_run("sudo apk add ncdu");
        let screen = format!(
            "{}\n[sudo] password for linuxio:\nhunter2\n(1/1) Installing ncdu\n{}0>>>\n",
            run.start_tag(),
            run.exit_tag()
        );
        let out = ai_term_result(&screen, &run).expect("marker present");
        assert!(out.contains("hunter2"), "the screen itself still has it: {out}");
        let scrubbed = scrub_secret(out, Some("hunter2"));
        assert!(!scrubbed.contains("hunter2"), "{scrubbed}");
        assert!(scrubbed.contains("[REDACTED]"), "{scrubbed}");
        assert!(scrubbed.contains("Installing ncdu"), "{scrubbed}");
        // No secret configured → text is untouched.
        assert_eq!(scrub_secret("plain".into(), None), "plain");
    }

    /// A failed install gets the host's real package manager appended, and the
    /// shell's probe lines are stripped from the model-visible output.
    #[test]
    fn terminal_run_strips_probe_and_hints_the_real_package_manager() {
        let run = sample_term_run("sudo apt-get install -y htop");
        let screen = format!(
            "{}\nsudo: apt-get: command not found\n{}\n/sbin/apk\n{}1>>>\nuser@host:~$ ",
            run.start_tag(),
            run.pm_tag(),
            run.exit_tag()
        );
        let out = ai_term_result(&screen, &run).expect("marker present");
        assert!(out.starts_with("[exit 1]"), "{out}");
        assert!(
            !out.contains(&run.pm_tag()),
            "probe marker must be stripped: {out}"
        );
        assert!(
            !out.contains("/sbin/apk"),
            "probe output must be stripped: {out}"
        );
        assert!(out.contains("apk (Alpine)"), "{out}");
        assert!(out.contains("`sudo apk add <package>`"), "{out}");
    }
}
