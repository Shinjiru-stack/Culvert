//! The AI Copilot panel: chat surface, approval cards, security gates,
//! execution/audit history and the provider settings modal.
//!
//! All UI state lives here; the engine task is reached only through the
//! command/event channels created in [`AiPanel::new`].

use crate::ai::config::{self, AiConfig, SecMode};
use crate::ai::engine::{AiCmd, AiEvent};
use crate::ai::provider::Flavor;
use crate::ai::security::Risk;
use crate::ai::{new_context, new_audit, push_audit, AuditEntry, SecretPrompt, SharedAudit, UiAction, UiContext};
use crate::log::SharedLogs;
use crate::theme::{self, tglyph};
use eframe::egui::{self, Align2, Color32, CornerRadius, Frame, Margin, RichText, Ui};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Chat,
    History,
}

pub struct UiMsg {
    role: Role,
    text: String,
    card: Option<Card>,
}

#[derive(Clone, Copy, PartialEq)]
enum Role {
    User,
    Assistant,
    Info,
    Error,
}

#[derive(Clone, Copy, PartialEq)]
enum CardStatus {
    Running,
    Ok,
    Err,
}

struct Card {
    tool: String,
    summary: String,
    status: CardStatus,
    result: String,
    expanded: bool,
}

pub struct ExecRecord {
    time: String,
    tool: String,
    summary: String,
    outcome: String,
    ok: bool,
}

enum PendingKind {
    Approval {
        tool: String,
        summary: String,
        command: Option<String>,
        risk: Risk,
        reason: String,
        secret: Option<SecretPrompt>,
    },
    Gate {
        source: String,
        hits: Vec<String>,
        original_preview: String,
        redacted_preview: String,
    },
}

struct Pending {
    id: u64,
    kind: PendingKind,
}

/// In-flight GitHub device login. The token only lands in `draft.api_key`
/// after the user authorizes in the browser; it never enters chat or audit.
struct GitHubLogin {
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    status: String,
    error: Option<String>,
}

enum GitHubLoginEvent {
    DeviceCode {
        user_code: String,
        verification_uri: String,
        expires_in: u64,
    },
    Done {
        token: String,
    },
    Error(String),
}

pub struct AiPanel {
    pub open: bool,
    pub busy: bool,
    pub cfg: AiConfig,
    pub snapshot: Arc<RwLock<UiContext>>,
    pub audit: SharedAudit,

    settings_open: bool,
    draft: AiConfig,
    models: Vec<String>,
    models_note: Option<(bool, String)>,
    test_note: Option<(bool, String)>,
    api_key_visible: bool,
    github_login: Option<GitHubLogin>,
    github_rx: Option<UnboundedReceiver<GitHubLoginEvent>>,
    github_task: Option<tokio::task::JoinHandle<()>>,

    msgs: Vec<UiMsg>,
    streaming: Option<String>,
    input: String,
    pending: Option<Pending>,
    edit_command: bool,
    edit_buf: String,
    secret_buf: String,
    secret_visible: bool,
    tab: Tab,
    records: Vec<ExecRecord>,
    pending_action: Option<(u64, UiAction, String)>,
    /// Follow the newest reply. Cleared when the user scrolls up, restored by
    /// the jump-to-latest button (or by scrolling back to the bottom).
    chat_follow: bool,
    /// Force a scroll on the next frame, used by the jump button.
    chat_scroll_latest: bool,
    /// Content size markers, so a streaming reply can request a scroll only
    /// when something actually changed.
    chat_last_msgs: usize,
    chat_last_stream: usize,
    chat_last_pending: bool,

    cmd_tx: UnboundedSender<AiCmd>,
    evt_rx: UnboundedReceiver<AiEvent>,
    engine_started: bool,
    rt: tokio::runtime::Handle,
    logs: SharedLogs,
}

impl AiPanel {
    pub fn new(rt: tokio::runtime::Handle, logs: SharedLogs) -> Self {
        let (cmd_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let (_, evt_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut me = Self {
            open: false,
            busy: false,
            cfg: config::load(),
            snapshot: new_context(),
            audit: new_audit(),
            settings_open: false,
            draft: AiConfig::default(),
            models: Vec::new(),
            models_note: None,
            test_note: None,
            api_key_visible: false,
            github_login: None,
            github_rx: None,
            github_task: None,
            msgs: Vec::new(),
            streaming: None,
            input: String::new(),
            pending: None,
            edit_command: false,
            edit_buf: String::new(),
            secret_buf: String::new(),
            secret_visible: false,
            tab: Tab::Chat,
            records: Vec::new(),
            pending_action: None,
            chat_follow: true,
            chat_scroll_latest: false,
            chat_last_msgs: 0,
            chat_last_stream: 0,
            chat_last_pending: false,
            cmd_tx,
            evt_rx,
            engine_started: false,
            rt,
            logs,
        };
        me.msgs.push(UiMsg {
            role: Role::Info,
            text: "Ask about your servers in plain language — e.g. “check disk space on the demo host”, “show running containers”, “add an SSH server called staging for deploy@10.0.0.9”, “add a tunnel to staging from local 15433 to 10.0.0.20:5432”.\n\nI run read-only commands straight away; anything that saves or changes state asks for your approval first. Passwords and passphrases are typed into a masked field on the approval card — they are never sent to the model.".into(),
            card: None,
        });
        me
    }

    /// True when no model is configured yet (panel should open settings).
    pub fn needs_setup(&self) -> bool {
        self.cfg.model.trim().is_empty()
    }

    pub fn open_settings(&mut self) {
        self.cancel_github_login();
        self.draft = self.cfg.clone();
        if Flavor::from_id(&self.draft.provider) == Flavor::GitHubCopilot {
            self.draft.base_url.clear();
        }
        self.api_key_visible = false;
        self.settings_open = true;
    }

    pub fn provider_line(&self) -> String {
        format!(
            "{} · {}",
            Flavor::from_id(&self.cfg.provider).label(),
            if self.cfg.model.trim().is_empty() {
                "(no model)"
            } else {
                self.cfg.model.trim()
            }
        )
    }

    /// Spawn the engine task on first use (needs an egui context to wake the
    /// UI from async events).
    pub fn ensure_engine(&mut self, ctx: &egui::Context) {
        if self.engine_started {
            return;
        }
        self.engine_started = true;
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();
        self.cmd_tx = cmd_tx;
        self.evt_rx = evt_rx;
        let engine = crate::ai::engine::Engine::new(
            self.cfg.clone(),
            self.snapshot.clone(),
            self.logs.clone(),
            self.audit.clone(),
            cmd_rx,
            evt_tx,
            ctx.clone(),
        );
        self.rt.spawn(engine.run());
    }

    fn send(&self, cmd: AiCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    pub fn take_action(&mut self) -> Option<(u64, UiAction, String)> {
        self.pending_action.take()
    }

    pub fn complete_action(&mut self, id: u64, result: String) {
        self.send(AiCmd::ActionDone { id, result });
    }

    fn send_user(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.input.clear();
        self.msgs.push(UiMsg {
            role: Role::User,
            text: text.clone(),
            card: None,
        });
        self.busy = true;
        self.chat_follow = true;
        self.chat_scroll_latest = true;
        self.send(AiCmd::UserMessage(text));
    }

    /// Start GitHub's device flow. The browser opens as soon as GitHub returns
    /// the one-time code; the token is delivered back through `github_rx`.
    fn start_github_login(&mut self, ctx: &egui::Context) {
        self.cancel_github_login();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.github_rx = Some(rx);
        self.github_login = Some(GitHubLogin {
            user_code: String::new(),
            verification_uri: String::new(),
            expires_in: 900,
            status: "Requesting a GitHub device code…".into(),
            error: None,
        });

        let code_ctx = ctx.clone();
        let done_ctx = ctx.clone();
        let handle = self.rt.spawn(async move {
            let code_tx = tx.clone();
            let result = crate::ai::provider::github_device_login(move |code, uri, expires| {
                let _ = code_tx.send(GitHubLoginEvent::DeviceCode {
                    user_code: code.to_string(),
                    verification_uri: uri.to_string(),
                    expires_in: expires,
                });
                code_ctx.request_repaint();
            })
            .await;
            match result {
                Ok(token) => {
                    let _ = tx.send(GitHubLoginEvent::Done { token });
                }
                Err(e) => {
                    let _ = tx.send(GitHubLoginEvent::Error(e));
                }
            }
            done_ctx.request_repaint();
        });
        self.github_task = Some(handle);
    }

    fn cancel_github_login(&mut self) {
        if let Some(task) = self.github_task.take() {
            task.abort();
        }
        self.github_rx = None;
        self.github_login = None;
    }

    fn poll_github_login(&mut self) -> bool {
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = &mut self.github_rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
            disconnected = rx.is_closed();
        }
        if events.is_empty() {
            if !disconnected {
                return false;
            }
            if let Some(login) = &mut self.github_login {
                login.status = "Sign-in failed".into();
                login.error =
                    Some("GitHub sign-in ended unexpectedly. Try again.".to_string());
            }
            self.github_task = None;
            self.github_rx = None;
            return true;
        }

        let mut finished = false;
        for ev in events {
            match ev {
                GitHubLoginEvent::DeviceCode {
                    user_code,
                    verification_uri,
                    expires_in,
                } => {
                    if is_github_url(&verification_uri) {
                        open_url(&verification_uri);
                    }
                    if let Some(login) = &mut self.github_login {
                        login.user_code = user_code;
                        login.verification_uri = verification_uri;
                        login.expires_in = expires_in;
                        login.status = "Waiting for authorization in the browser…".into();
                    }
                }
                GitHubLoginEvent::Done { token } => {
                    self.draft.api_key = token;
                    self.github_login = None;
                    self.github_task = None;
                    finished = true;
                    self.models_note =
                        Some((true, "Signed in to GitHub — fetching Copilot models…".into()));
                    self.send(AiCmd::FetchModels(Box::new(self.draft.clone())));
                }
                GitHubLoginEvent::Error(e) => {
                    if let Some(login) = &mut self.github_login {
                        login.status = "Sign-in failed".into();
                        login.error = Some(e);
                    }
                    self.github_task = None;
                    finished = true;
                }
            }
        }
        if finished {
            self.github_rx = None;
        }
        true
    }

    // -----------------------------------------------------------------------
    // Event pump
    // -----------------------------------------------------------------------

    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.evt_rx.try_recv() {
            changed = true;
            match ev {
                AiEvent::TurnStart => {
                    self.busy = true;
                    self.streaming = Some(String::new());
                    self.pending = None;
                    self.edit_command = false;
                    self.secret_buf.clear();
                    self.secret_visible = false;
                }
                AiEvent::Delta(d) => {
                    self.streaming.get_or_insert_with(String::new).push_str(&d);
                }
                AiEvent::AssistantText(t) => {
                    self.streaming = None;
                    if !t.trim().is_empty() {
                        self.msgs.push(UiMsg {
                            role: Role::Assistant,
                            text: t,
                            card: None,
                        });
                    }
                }
                AiEvent::ToolStart { tool, summary } => {
                    self.msgs.push(UiMsg {
                        role: Role::Info,
                        text: String::new(),
                        card: Some(Card {
                            tool,
                            summary,
                            status: CardStatus::Running,
                            result: String::new(),
                            expanded: false,
                        }),
                    });
                }
                AiEvent::ToolResult {
                    tool,
                    summary,
                    ok,
                    text,
                } => {
                    // Update the most recent still-running card for this tool.
                    let mut updated = false;
                    for m in self.msgs.iter_mut().rev() {
                        if let Some(c) = &mut m.card {
                            if c.tool == tool && c.status == CardStatus::Running {
                                c.status = if ok { CardStatus::Ok } else { CardStatus::Err };
                                c.result = text.clone();
                                updated = true;
                                break;
                            }
                        }
                    }
                    if !updated {
                        self.msgs.push(UiMsg {
                            role: Role::Info,
                            text: String::new(),
                            card: Some(Card {
                                tool: tool.clone(),
                                summary: summary.clone(),
                                status: if ok { CardStatus::Ok } else { CardStatus::Err },
                                result: text.clone(),
                                expanded: false,
                            }),
                        });
                    }
                    self.records.push(ExecRecord {
                        time: chrono::Local::now().format("%H:%M:%S").to_string(),
                        tool,
                        summary,
                        outcome: clip(&text, 160),
                        ok,
                    });
                }
                AiEvent::Approval {
                    id,
                    tool,
                    summary,
                    command,
                    risk,
                    reason,
                    secret,
                } => {
                    self.edit_command = false;
                    self.edit_buf = command.clone().unwrap_or_default();
                    self.secret_buf.clear();
                    self.secret_visible = false;
                    self.pending = Some(Pending {
                        id,
                        kind: PendingKind::Approval {
                            tool,
                            summary,
                            command,
                            risk,
                            reason,
                            secret,
                        },
                    });
                }
                AiEvent::SensitiveGate {
                    id,
                    source,
                    hits,
                    original_preview,
                    redacted_preview,
                } => {
                    self.pending = Some(Pending {
                        id,
                        kind: PendingKind::Gate {
                            source,
                            hits,
                            original_preview,
                            redacted_preview,
                        },
                    });
                }
                AiEvent::Action { id, action, label } => {
                    self.pending_action = Some((id, action, label));
                }
                AiEvent::Info(t) => {
                    self.msgs.push(UiMsg {
                        role: Role::Info,
                        text: t,
                        card: None,
                    });
                }
                AiEvent::Error(t) => {
                    self.msgs.push(UiMsg {
                        role: Role::Error,
                        text: t,
                        card: None,
                    });
                }
                AiEvent::TurnEnd => {
                    self.busy = false;
                    self.pending = None;
                    self.secret_buf.clear();
                    self.secret_visible = false;
                    if let Some(s) = self.streaming.take() {
                        if !s.trim().is_empty() {
                            self.msgs.push(UiMsg {
                                role: Role::Assistant,
                                text: s,
                                card: None,
                            });
                        }
                    }
                }
                AiEvent::Models(list, note) => {
                    let ok = !list.is_empty();
                    self.models = list;
                    if ok
                        && Flavor::from_id(&self.draft.provider) == Flavor::GitHubCopilot
                        && self.draft.model.trim().is_empty()
                    {
                        self.draft.model = self.models[0].clone();
                    }
                    self.models_note = Some((ok, note));
                }
                AiEvent::TestResult(ok, msg) => {
                    self.test_note = Some((ok, msg));
                }
            }
        }
        if self.poll_github_login() {
            changed = true;
        }
        changed
    }

    // -----------------------------------------------------------------------
    // Panel UI
    // -----------------------------------------------------------------------

    pub fn panel_ui(&mut self, ui: &mut Ui, _ctx: &egui::Context) {
        self.header(ui);
        ui.add_space(4.0);
        ui.separator();

        // Pin the composer to the bottom in its own child Ui. Reserving it
        // before the transcript (rather than laying it out after the
        // ScrollArea) keeps it exactly where it is painted no matter how wide
        // the conversation content grows.
        egui::TopBottomPanel::bottom("ai_composer")
            .frame(egui::Frame::NONE)
            .show_inside(ui, |ui| {
                ui.add_space(2.0);
                self.input_row(ui);
            });

        // Scroll behaviour: follow a streaming reply until the user scrolls
        // up, then offer a jump-to-latest button. Scrolling back to the bottom
        // re-arms following automatically.
        let chat_rect = ui.available_rect_before_wrap();
        let wheel_up = self.tab == Tab::Chat
            && ui.input(|i| {
                i.raw_scroll_delta.y > 0.5
                    && i.pointer
                        .hover_pos()
                        .is_some_and(|p| chat_rect.contains(p))
            });
        if wheel_up {
            self.chat_follow = false;
        }

        let stream_len = self.streaming.as_ref().map_or(0, |s| s.len());
        let has_pending = self.pending.is_some();
        let new_content = self.msgs.len() != self.chat_last_msgs
            || stream_len != self.chat_last_stream
            || has_pending != self.chat_last_pending;
        self.chat_last_msgs = self.msgs.len();
        self.chat_last_stream = stream_len;
        self.chat_last_pending = has_pending;

        let scroll_latest = self.chat_scroll_latest || (self.chat_follow && new_content);
        self.chat_scroll_latest = false;
        let mut area = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(self.chat_follow);
        if scroll_latest {
            // Clamp to the end; `stick_to_bottom` finishes the job once this
            // frame's content height is known.
            area = area.vertical_scroll_offset(f32::MAX);
        }
        let out = area.show(ui, |ui| match self.tab {
            Tab::Chat => self.chat_body(ui),
            Tab::History => self.history_body(ui),
        });

        let max_offset = (out.content_size.y - out.inner_rect.height()).max(0.0);
        let at_bottom = max_offset <= 1.0 || out.state.offset.y >= max_offset - 2.0;
        if at_bottom {
            self.chat_follow = true;
        }
        if self.tab == Tab::Chat && !at_bottom && max_offset > 20.0 {
            let btn_rect = egui::Rect::from_center_size(
                egui::pos2(
                    out.inner_rect.right() - 20.0,
                    out.inner_rect.bottom() - 20.0,
                ),
                egui::vec2(32.0, 32.0),
            );
            let jump = ui.put(
                btn_rect,
                egui::Button::new(
                    RichText::new(tglyph::CHEVRON_DOWN)
                        .font(theme::icon(16.0))
                        .color(Color32::WHITE),
                )
                .fill(theme::ACCENT())
                .corner_radius(CornerRadius::same(16))
                .min_size(egui::vec2(32.0, 32.0)),
            );
            if jump.on_hover_text("Jump to the latest reply").clicked() {
                self.chat_follow = true;
                self.chat_scroll_latest = true;
                ui.ctx().request_repaint();
            }
        }
    }

    fn header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(tglyph::COPILOT)
                    .font(theme::icon(15.0))
                    .color(theme::VIOLET()),
            );
            ui.label(RichText::new("AI Copilot").strong().size(13.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icon_button(ui, tglyph::CLOSE, "Hide the Copilot panel").clicked() {
                    self.open = false;
                }
                if icon_button(ui, tglyph::GEAR, "AI settings").clicked() {
                    self.open_settings();
                }
                let hist_active = self.tab == Tab::History;
                let r = ui.add(
                    egui::Button::new(
                        RichText::new(tglyph::HISTORY)
                            .font(theme::icon(14.0))
                            .color(if hist_active {
                                theme::ACCENT()
                            } else {
                                theme::TEXT_DIM()
                            }),
                    )
                    .frame(false),
                );
                if r.clicked() {
                    self.tab = if hist_active { Tab::Chat } else { Tab::History };
                }
                r.on_hover_text("Execution + audit history");
            });
        });
        ui.horizontal(|ui| {
            let dot = if self.busy {
                theme::WARNING()
            } else {
                theme::SUCCESS()
            };
            ui.label(RichText::new("●").size(9.0).color(dot));
            ui.label(
                RichText::new(self.provider_line())
                    .size(10.5)
                    .color(theme::TEXT_DIM()),
            );
        });
    }

    fn chat_body(&mut self, ui: &mut Ui) {
        let mut act: Option<UiCmd> = None;
        let mut expand_toggles: Vec<usize> = Vec::new();

        for (i, m) in self.msgs.iter_mut().enumerate() {
            match m.role {
                Role::User => {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        egui::Frame::NONE
                            .fill(theme::ACCENT().gamma_multiply(0.18))
                            .corner_radius(CornerRadius::same(6))
                            .inner_margin(Margin::symmetric(8, 5))
                            .show(ui, |ui| {
                                // egui extends text instead of wrapping inside
                                // horizontal layouts, so cap the bubble and ask
                                // for wrapping explicitly.
                                ui.set_max_width((ui.available_width() * 0.88).max(80.0));
                                ui.add(
                                    egui::Label::new(RichText::new(&m.text).size(12.0))
                                        .wrap_mode(egui::TextWrapMode::Wrap),
                                );
                            });
                    });
                    ui.add_space(6.0);
                }
                Role::Assistant => {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(tglyph::BOT)
                                .font(theme::icon(13.0))
                                .color(theme::ACCENT()),
                        );
                        ui.vertical(|ui| md_ui(ui, &m.text, theme::TEXT()));
                    });
                    ui.add_space(6.0);
                }
                Role::Info => {
                    if let Some(card) = &mut m.card {
                        card_ui(ui, card, &mut expand_toggles, i);
                        ui.add_space(4.0);
                    } else if !m.text.is_empty() {
                        ui.label(
                            RichText::new(&m.text)
                                .size(11.0)
                                .color(theme::TEXT_FAINT()),
                        );
                        ui.add_space(4.0);
                    }
                }
                Role::Error => {
                    egui::Frame::NONE
                        .fill(theme::BG_PANEL())
                        .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(196, 84, 76)))
                        .corner_radius(CornerRadius::same(6))
                        .inner_margin(Margin::symmetric(8, 5))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(&m.text)
                                    .size(11.5)
                                    .color(Color32::from_rgb(210, 96, 88)),
                            );
                        });
                    ui.add_space(4.0);
                }
            }
        }

        // Streaming bubble.
        if let Some(s) = &self.streaming {
            if !s.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(tglyph::BOT)
                            .font(theme::icon(13.0))
                            .color(theme::ACCENT()),
                    );
                    ui.vertical(|ui| md_ui(ui, s, theme::TEXT()));
                });
            }
        }

        // Apply card expand/collapse clicks collected during rendering.
        for i in expand_toggles {
            if let Some(m) = self.msgs.get_mut(i) {
                if let Some(c) = &mut m.card {
                    c.expanded = !c.expanded;
                }
            }
        }

        // Pending approval / security gate.
        if let Some(p) = &self.pending {
            ui.add_space(6.0);
            match &p.kind {
                PendingKind::Approval {
                    tool,
                    summary,
                    command,
                    risk,
                    reason,
                    secret,
                } => {
                    let colour = match risk {
                        Risk::High => Color32::from_rgb(196, 84, 76),
                        _ => Color32::from_rgb(190, 148, 42),
                    };
                    Frame::NONE
                        .fill(theme::BG_PANEL())
                        .stroke(egui::Stroke::new(1.0_f32, colour))
                        .corner_radius(CornerRadius::same(6))
                        .inner_margin(Margin::symmetric(9, 7))
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new(tglyph::SHIELD)
                                        .font(theme::icon(13.0))
                                        .color(colour),
                                );
                                ui.label(
                                    RichText::new(format!("{} — {}", risk.label(), reason))
                                        .strong()
                                        .size(11.5)
                                        .color(colour),
                                );
                            });
                            ui.add_space(3.0);
                            ui.label(
                                RichText::new(format!("{tool} · {summary}")).size(11.5),
                            );
                            if let Some(cmd) = command {
                                ui.add_space(3.0);
                                if self.edit_command {
                                    ui.add(
                                        egui::TextEdit::multiline(&mut self.edit_buf)
                                            .font(egui::TextStyle::Monospace)
                                            .desired_rows(2)
                                            .desired_width(f32::INFINITY),
                                    );
                                } else {
                                    egui::Frame::NONE
                                        .fill(theme::BG_WINDOW())
                                        .corner_radius(CornerRadius::same(4))
                                        .inner_margin(Margin::symmetric(6, 4))
                                        .show(ui, |ui| {
                                            ui.label(
                                                RichText::new(cmd.as_str())
                                                    .monospace()
                                                    .size(11.0),
                                            );
                                        });
                                }
                            }
                            if let Some(spec) = secret {
                                ui.add_space(5.0);
                                ui.label(
                                    RichText::new(format!(
                                        "{} — stored locally, never sent to the model",
                                        spec.label
                                    ))
                                    .size(10.5)
                                    .color(theme::TEXT_DIM()),
                                );
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.secret_buf)
                                            .password(!self.secret_visible)
                                            .hint_text(spec.hint.as_str())
                                            .desired_width((ui.available_width() - 30.0).max(80.0)),
                                    );
                                    let (glyph, tip) = if self.secret_visible {
                                        (tglyph::EYE_OFF, "Hide password")
                                    } else {
                                        (tglyph::EYE, "Show password")
                                    };
                                    if icon_button(ui, glyph, tip).clicked() {
                                        self.secret_visible = !self.secret_visible;
                                    }
                                });
                            }
                            let secret_missing = secret.as_ref().is_some_and(|s| {
                                s.required && self.secret_buf.trim().is_empty()
                            });
                            if secret_missing {
                                ui.label(
                                    RichText::new("Enter the credential above to enable Execute.")
                                        .size(10.0)
                                        .color(theme::WARNING()),
                                );
                            }
                            ui.add_space(5.0);
                            ui.horizontal(|ui| {
                                let exec = if self.edit_command {
                                    "Execute edited"
                                } else {
                                    "Execute"
                                };
                                if ui
                                    .add_enabled(
                                        !secret_missing,
                                        egui::Button::new(
                                            RichText::new(exec).size(11.5).color(Color32::WHITE),
                                        )
                                        .fill(theme::SUCCESS()),
                                    )
                                    .clicked()
                                {
                                    let edited = if self.edit_command
                                        && !self.edit_buf.trim().is_empty()
                                    {
                                        Some(self.edit_buf.clone())
                                    } else {
                                        None
                                    };
                                    let secret = secret
                                        .as_ref()
                                        .map(|_| self.secret_buf.clone());
                                    act = Some(UiCmd::Approve { id: p.id, edited, secret });
                                }
                                if command.is_some()
                                    && ui
                                        .add(egui::Button::new(
                                            RichText::new(if self.edit_command {
                                                "Revert"
                                            } else {
                                                "Edit"
                                            })
                                            .size(11.5),
                                        ))
                                        .clicked()
                                {
                                    self.edit_command = !self.edit_command;
                                    if !self.edit_command {
                                        if let Some(c) = command {
                                            self.edit_buf = c.clone();
                                        }
                                    }
                                }
                                if ui
                                    .add(egui::Button::new(RichText::new("Cancel").size(11.5)))
                                    .clicked()
                                {
                                    act = Some(UiCmd::CancelTurn);
                                }
                            });
                        });
                }
                PendingKind::Gate {
                    source,
                    hits,
                    original_preview,
                    redacted_preview,
                } => {
                    let colour = Color32::from_rgb(196, 84, 76);
                    Frame::NONE
                        .fill(theme::BG_PANEL())
                        .stroke(egui::Stroke::new(1.0_f32, colour))
                        .corner_radius(CornerRadius::same(6))
                        .inner_margin(Margin::symmetric(9, 7))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(tglyph::SHIELD)
                                        .font(theme::icon(13.0))
                                        .color(colour),
                                );
                                ui.label(
                                    RichText::new("Sensitive content detected")
                                        .strong()
                                        .size(11.5)
                                        .color(colour),
                                );
                            });
                            ui.label(RichText::new(source).size(11.0));
                            for h in hits {
                                ui.label(
                                    RichText::new(format!("• {h}"))
                                        .size(10.5)
                                        .color(theme::TEXT_DIM()),
                                );
                            }
                            ui.add_space(3.0);
                            ui.collapsing("Redacted preview", |ui| {
                                ui.label(RichText::new(mask_preview(redacted_preview)).monospace().size(10.0));
                            });
                            ui.collapsing("Original preview", |ui| {
                                ui.label(RichText::new(mask_preview(original_preview)).monospace().size(10.0));
                            });
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .add(egui::Button::new(RichText::new("Send original").size(11.5)))
                                    .clicked()
                                {
                                    act = Some(UiCmd::Approve {
                                        id: p.id,
                                        edited: None,
                                        secret: None,
                                    });
                                }
                                if ui
                                    .add(egui::Button::new(
                                        RichText::new("Send redacted")
                                            .size(11.5)
                                            .color(Color32::WHITE),
                                    ).fill(theme::ACCENT()))
                                    .clicked()
                                {
                                    act = Some(UiCmd::Approve {
                                        id: p.id,
                                        edited: Some("redacted".into()),
                                        secret: None,
                                    });
                                }
                                if ui
                                    .add(egui::Button::new(RichText::new("Cancel").size(11.5)))
                                    .clicked()
                                {
                                    act = Some(UiCmd::Reject { id: p.id });
                                }
                            });
                        });
                }
            }
        }

        if let Some((_, _, label)) = &self.pending_action {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("… {label}"))
                    .size(11.0)
                    .color(theme::WARNING()),
            );
        }

        if let Some(act) = act {
            self.pending = None;
            self.secret_buf.clear();
            self.secret_visible = false;
            match act {
                UiCmd::Approve { id, edited, secret } => {
                    self.send(AiCmd::Approve { id, edited, secret })
                }
                UiCmd::Reject { id } => self.send(AiCmd::Reject { id }),
                UiCmd::CancelTurn => self.send(AiCmd::Cancel),
            }
        }
    }

    fn history_body(&mut self, ui: &mut Ui) {
        if self.records.is_empty() {
            ui.label(
                RichText::new("No tool executions yet.")
                    .size(11.0)
                    .color(theme::TEXT_FAINT()),
            );
        } else {
            ui.label(RichText::new("Executions").strong().size(11.5));
            for r in self.records.iter().rev().take(60) {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(&r.time)
                            .size(10.0)
                            .color(theme::TEXT_FAINT()),
                    );
                    ui.label(
                        RichText::new(&r.tool)
                            .monospace()
                            .size(10.5)
                            .color(if r.ok { theme::TEXT() } else { Color32::from_rgb(210, 96, 88) }),
                    );
                });
                if !r.summary.is_empty() {
                    ui.label(
                        RichText::new(clip(&r.summary, 160))
                            .size(10.0)
                            .color(theme::TEXT_DIM()),
                    );
                }
                ui.label(RichText::new(&r.outcome).size(10.0).color(theme::TEXT_DIM()));
                ui.add_space(3.0);
            }
        }
        ui.add_space(8.0);
        ui.separator();
        ui.label(RichText::new("Audit trail").strong().size(11.5));
        let entries: Vec<AuditEntry> = self
            .audit
            .lock()
            .map(|w| w.iter().rev().take(120).cloned().collect())
            .unwrap_or_default();
        if entries.is_empty() {
            ui.label(
                RichText::new("Nothing logged yet.")
                    .size(10.5)
                    .color(theme::TEXT_FAINT()),
            );
        }
        for e in entries {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!("{} [{}]", e.time, e.kind))
                        .size(9.5)
                        .color(theme::TEXT_FAINT()),
                );
                ui.label(RichText::new(clip(&e.detail, 220)).size(10.0));
            });
        }
    }

    fn input_row(&mut self, ui: &mut Ui) {
        let hint = if self.busy {
            "Working… (Stop cancels approvals; wait for commands to finish)"
        } else {
            "Ask anything — “check disk space”, “add a server”, “add a tunnel to staging”…"
        };
        let resp = ui.add(
            egui::TextEdit::multiline(&mut self.input)
                .hint_text(hint)
                .desired_rows(2)
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Small),
        );
        let enter_sends = resp.has_focus()
            && ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift);
        ui.horizontal(|ui| {
            if self.busy {
                if ui
                    .add(egui::Button::new(RichText::new("Stop").size(11.5)))
                    .clicked()
                {
                    self.send(AiCmd::Cancel);
                }
            } else {
                let send = ui.add(
                    egui::Button::new(
                        RichText::new(format!("{}  Send", tglyph::SEND))
                            .size(11.5)
                            .color(Color32::WHITE),
                    )
                    .fill(theme::ACCENT()),
                );
                if send.clicked() || enter_sends {
                    self.send_user();
                }
            }
            if ui
                .add(egui::Button::new(RichText::new("New chat").size(11.0)))
                .clicked()
            {
                self.msgs.clear();
                self.records.clear();
                self.send(AiCmd::NewChat);
            }
            ui.label(
                RichText::new(if self.busy { "● running" } else { "" })
                    .size(10.0)
                    .color(theme::WARNING()),
            );
        });
    }

    /// GitHub Copilot is a subscription, not an API key: the primary path is
    /// GitHub's device login. The token field stays available for automation.
    fn github_provider_ui(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        ui.label(RichText::new("GitHub Copilot subscription").size(11.5).strong());
        let signed_in = !self.draft.api_key.trim().is_empty();
        let pending = self.github_login.is_some();

        if signed_in {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Signed in — Save keeps the token encrypted with Windows DPAPI.")
                        .size(10.0)
                        .color(theme::SUCCESS()),
                );
                if ui
                    .small_button("Sign out")
                    .on_hover_text("Save to apply")
                    .clicked()
                {
                    self.draft.api_key.clear();
                    self.cancel_github_login();
                    self.models.clear();
                    self.models_note = None;
                }
                if ui.small_button("Sign in again").clicked() {
                    self.draft.api_key.clear();
                    self.start_github_login(ctx);
                }
            });
        } else if !pending {
            ui.label(
                RichText::new(
                    "Sign in with the GitHub account that has a Copilot Pro, Pro+, Business or Enterprise plan.",
                )
                .size(9.5)
                .color(theme::TEXT_FAINT()),
            );
            if ui.button(RichText::new("Sign in with GitHub").size(11.0)).clicked() {
                self.start_github_login(ctx);
            }
        }

        let mut cancel_login = false;
        let mut retry_login = false;
        if let Some(login) = &self.github_login {
            ui.add_space(3.0);
            if let Some(err) = &login.error {
                ui.label(
                    RichText::new(err)
                        .size(10.0)
                        .color(Color32::from_rgb(210, 96, 88)),
                );
            } else if login.user_code.is_empty() {
                ui.label(
                    RichText::new(&login.status)
                        .size(10.0)
                        .color(theme::TEXT_DIM()),
                );
            } else {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Code").size(10.0).strong());
                    ui.label(
                        RichText::new(&login.user_code)
                            .monospace()
                            .size(14.0)
                            .strong(),
                    );
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(login.user_code.clone());
                    }
                });
                ui.hyperlink_to("Open github.com/login/device", &login.verification_uri);
                let mins = (login.expires_in + 59) / 60;
                ui.label(
                    RichText::new(format!(
                        "Waiting for authorization in the browser… code expires in about {mins} min."
                    ))
                    .size(9.5)
                    .color(theme::TEXT_FAINT()),
                );
            }
            if login.error.is_some() {
                ui.horizontal(|ui| {
                    if ui.small_button("Try again").clicked() {
                        retry_login = true;
                    }
                    if ui.small_button("Dismiss").clicked() {
                        cancel_login = true;
                    }
                });
            } else if ui.small_button("Cancel sign-in").clicked() {
                cancel_login = true;
            }
        }
        if cancel_login {
            self.cancel_github_login();
        }
        if retry_login {
            self.draft.api_key.clear();
            self.start_github_login(ctx);
        }

        ui.collapsing("Advanced: use an existing GitHub token", |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.draft.api_key)
                        .password(!self.api_key_visible)
                        .desired_width((ui.available_width() - 30.0).max(80.0))
                        .hint_text("gho_…, ghu_… or a Copilot token"),
                );
                let (glyph, tip) = if self.api_key_visible {
                    (tglyph::EYE_OFF, "Hide GitHub token")
                } else {
                    (tglyph::EYE, "Show GitHub token")
                };
                if icon_button(ui, glyph, tip).clicked() {
                    self.api_key_visible = !self.api_key_visible;
                }
            });
            ui.label(
                RichText::new(
                    "Only for automation. The normal path is Sign in with GitHub; a token entered here is sealed with DPAPI when you press Save.",
                )
                .size(9.5)
                .color(theme::TEXT_FAINT()),
            );
        });
    }

    // -----------------------------------------------------------------------
    // Settings modal
    // -----------------------------------------------------------------------

    /// Draw the settings window. Returns true when the user saved.
    pub fn settings_modal(&mut self, ctx: &egui::Context) -> bool {
        if !self.settings_open {
            return false;
        }
        let mut saved = false;
        let mut open = true;
        egui::Window::new("AI Copilot · Settings")
            .collapsible(false)
            .resizable(true)
            .default_width(470.0)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(520.0).show(ui, |ui| {
                    let flavor = Flavor::from_id(&self.draft.provider);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Provider").size(11.5).strong());
                        egui::ComboBox::from_id_salt("ai_provider")
                            .selected_text(flavor.label())
                            .width(230.0)
                            .show_ui(ui, |ui| {
                                for f in Flavor::ALL {
                                    if ui
                                        .selectable_label(
                                            self.draft.provider == f.id(),
                                            f.label(),
                                        )
                                        .clicked()
                                        && self.draft.provider != f.id()
                                    {
                                        // Stash the outgoing provider's key,
                                        // endpoint and model under its id, then
                                        // load the target provider's saved
                                        // values — switching back later must
                                        // not lose the earlier key or model.
                                        let old = std::mem::take(&mut self.draft.provider);
                                        self.draft.providers.insert(
                                            old,
                                            config::ProviderPrefs {
                                                api_key: std::mem::take(&mut self.draft.api_key),
                                                base_url: std::mem::take(&mut self.draft.base_url),
                                                model: std::mem::take(&mut self.draft.model),
                                            },
                                        );
                                        let prefs = self
                                            .draft
                                            .providers
                                            .get(f.id())
                                            .cloned()
                                            .unwrap_or_default();
                                        self.draft.provider = f.id().to_string();
                                        self.draft.api_key = prefs.api_key;
                                        self.draft.base_url = prefs.base_url;
                                        self.draft.model = prefs.model;
                                        self.models.clear();
                                        self.models_note = None;
                                        self.test_note = None;
                                        self.api_key_visible = false;
                                        if f == Flavor::GitHubCopilot {
                                            // The endpoint comes from the
                                            // Copilot token exchange.
                                            self.draft.base_url.clear();
                                        } else {
                                            self.cancel_github_login();
                                        }
                                    }
                                }
                            });
                    });
                    ui.add_space(4.0);

                    if flavor == Flavor::GitHubCopilot {
                        self.github_provider_ui(ui, ctx);
                    } else {
                        ui.label(RichText::new("API key").size(11.5).strong());
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut self.draft.api_key)
                                    .password(!self.api_key_visible)
                                    .desired_width((ui.available_width() - 30.0).max(80.0))
                                    .hint_text(flavor.key_hint()),
                            );
                            let (glyph, tip) = if self.api_key_visible {
                                (tglyph::EYE_OFF, "Hide API key")
                            } else {
                                (tglyph::EYE, "Show API key")
                            };
                            if icon_button(ui, glyph, tip).clicked() {
                                self.api_key_visible = !self.api_key_visible;
                            }
                        });
                        ui.label(
                            RichText::new("Encrypted with Windows DPAPI (your Windows account is the key).")
                                .size(9.5)
                                .color(theme::TEXT_FAINT()),
                        );
                    }
                    ui.add_space(6.0);

                    if flavor != Flavor::GitHubCopilot {
                        ui.label(RichText::new("Base URL").size(11.5).strong());
                        ui.add(
                            egui::TextEdit::singleline(&mut self.draft.base_url)
                                .desired_width(f32::INFINITY)
                                .hint_text(flavor.default_base()),
                        );
                    } else {
                        ui.label(
                            RichText::new(
                                "Endpoint: discovered from your GitHub Copilot token (api.githubcopilot.com for individual plans).",
                            )
                            .size(9.5)
                            .color(theme::TEXT_FAINT()),
                        );
                    }
                    ui.add_space(6.0);

                    ui.label(RichText::new("Model").size(11.5).strong());
                    ui.horizontal(|ui| {
                        egui::ComboBox::from_id_salt("ai_model")
                            .selected_text(if self.draft.model.is_empty() {
                                "(not set)"
                            } else {
                                self.draft.model.as_str()
                            })
                            .width(240.0)
                            .show_ui(ui, |ui| {
                                for m in &self.models {
                                    if ui
                                        .selectable_label(self.draft.model == *m, m)
                                        .clicked()
                                    {
                                        self.draft.model = m.clone();
                                    }
                                }
                            });
                        if ui.button(RichText::new("Fetch models").size(11.0)).clicked() {
                            self.test_note = None;
                            self.send(AiCmd::FetchModels(Box::new(self.draft.clone())));
                        }
                    });
                    ui.add(
                        egui::TextEdit::singleline(&mut self.draft.model)
                            .desired_width(f32::INFINITY)
                            .hint_text("type a model id if the list is empty"),
                    );
                    if let Some((ok, note)) = &self.models_note {
                        ui.label(
                            RichText::new(note)
                                .size(10.0)
                                .color(if *ok { theme::TEXT_DIM() } else { Color32::from_rgb(210, 96, 88) }),
                        );
                    }
                    ui.add_space(6.0);

                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Temperature").size(11.5).strong());
                        ui.add(
                            egui::Slider::new(&mut self.draft.temperature, 0.0..=2.0)
                                .fixed_decimals(2),
                        );
                    });
                    ui.checkbox(
                        &mut self.draft.streaming,
                        RichText::new("Stream responses (OpenAI-compatible providers)").size(11.0),
                    );
                    ui.add_space(8.0);
                    ui.separator();

                    ui.label(RichText::new("Security mode").size(11.5).strong());
                    for m in SecMode::ALL {
                        ui.radio_value(&mut self.draft.security, m, m.label());
                        ui.label(
                            RichText::new(m.description())
                                .size(9.5)
                                .color(theme::TEXT_FAINT()),
                        );
                    }
                    if self.draft.provider == "ollama" {
                        ui.label(
                            RichText::new("Local model detected — redaction is relaxed; data stays on this machine.")
                                .size(9.5)
                                .color(theme::SUCCESS()),
                        );
                    }
                    ui.add_space(4.0);
                    ui.checkbox(
                        &mut self.draft.auto_safe,
                        RichText::new("Auto-approve read-only commands (ls, cat, df, grep, docker ps, …)").size(11.0),
                    );
                    ui.label(
                        RichText::new("State-changing and destructive commands always ask first.")
                            .size(9.5)
                            .color(theme::TEXT_FAINT()),
                    );
                    ui.add_space(8.0);
                    ui.separator();

                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Test connection").size(11.0)).clicked() {
                            self.test_note = None;
                            self.send(AiCmd::TestConnection(Box::new(self.draft.clone())));
                        }
                        if let Some((ok, msg)) = &self.test_note {
                            ui.label(
                                RichText::new(msg)
                                    .size(10.5)
                                    .color(if *ok { theme::SUCCESS() } else { Color32::from_rgb(210, 96, 88) }),
                            );
                        }
                    });
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Save").size(11.5).color(Color32::WHITE),
                                )
                                .fill(theme::ACCENT()),
                            )
                            .clicked()
                        {
                            self.cancel_github_login();
                            if Flavor::from_id(&self.draft.provider) == Flavor::GitHubCopilot {
                                self.draft.base_url.clear();
                            }
                            self.draft.providers.insert(
                                self.draft.provider.clone(),
                                config::ProviderPrefs {
                                    api_key: self.draft.api_key.clone(),
                                    base_url: self.draft.base_url.clone(),
                                    model: self.draft.model.clone(),
                                },
                            );
                            self.cfg = self.draft.clone();
                            match config::persist(&self.cfg) {
                                Ok(()) => {
                                    self.send(AiCmd::SetConfig(Box::new(self.cfg.clone())));
                                    push_audit(
                                        &self.audit,
                                        "info",
                                        format!(
                                            "settings saved: {} / {} / {}",
                                            self.cfg.provider,
                                            self.cfg.model,
                                            self.cfg.security.label()
                                        ),
                                    );
                                    saved = true;
                                    self.settings_open = false;
                                }
                                Err(e) => {
                                    self.test_note =
                                        Some((false, format!("could not save: {e}")));
                                }
                            }
                        }
                        if ui.button(RichText::new("Cancel").size(11.5)).clicked() {
                            self.cancel_github_login();
                            self.settings_open = false;
                        }
                    });
                });
            });
        if !open {
            self.cancel_github_login();
            self.settings_open = false;
        }
        saved
    }
}

enum UiCmd {
    Approve {
        id: u64,
        edited: Option<String>,
        secret: Option<String>,
    },
    Reject { id: u64 },
    CancelTurn,
}

// ---------------------------------------------------------------------------
// Small UI helpers
// ---------------------------------------------------------------------------

fn is_github_url(url: &str) -> bool {
    url.starts_with("https://github.com/") || url.starts_with("https://www.github.com/")
}

fn open_url(url: &str) {
    if !is_github_url(url) {
        return;
    }
    #[cfg(windows)]
    {
        // No shell: a URL must never be reparsed as a command line.
        let _ = std::process::Command::new("rundll32.exe")
            .args(["url.dll,FileProtocolHandler", url])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

fn icon_button(ui: &mut Ui, glyph: char, tip: &str) -> egui::Response {
    let r = ui.add(
        egui::Button::new(RichText::new(glyph.to_string()).font(theme::icon(13.0)))
            .frame(false),
    );
    r.on_hover_text(tip)
}

fn clip(s: &str, n: usize) -> String {
    let t = s.trim().replace('\n', " ");
    if t.chars().count() > n {
        let cut: String = t.chars().take(n).collect();
        format!("{cut}…")
    } else {
        t
    }
}

fn mask_preview(s: &str) -> String {
    clip(s, 700)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_copilot_settings_render_without_an_api_key() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let logs: SharedLogs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut panel = AiPanel::new(rt.handle().clone(), logs);
        panel.cfg.provider = "github-copilot".into();
        panel.cfg.api_key.clear();
        panel.cfg.base_url.clear();
        panel.cfg.model.clear();
        panel.draft = panel.cfg.clone();
        panel.settings_open = true;

        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 700.0),
            )),
            ..Default::default()
        };
        let output = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |_ui| {
                panel.settings_modal(ctx);
            });
        });
        assert!(!output.shapes.is_empty());
    }

    #[test]
    fn inline_code_uses_the_body_baseline_and_strips_markers() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(640.0, 480.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let job = inline_job(
                    ui,
                    "The highest CPU usage in `irun-app` is **steady**",
                    Color32::WHITE,
                );
                assert_eq!(
                    job.text,
                    "The highest CPU usage in irun-app is steady"
                );
                assert!(job.wrap.max_width > 1.0);
                assert!(job.sections.iter().any(|s| {
                    s.format.font_id.family == egui::FontFamily::Monospace
                }));
                assert!(job.sections.iter().any(|s| {
                    s.format.font_id.family == egui::FontFamily::Proportional
                }));

                // Laying the job out puts both fonts on the same baseline;
                // that is what stops inline code from floating above the line.
                let galley = ui.fonts(|f| f.layout_job(job));
                let row = &galley.rows[0];
                let body_y = row
                    .glyphs
                    .iter()
                    .find(|g| g.chr == 'T')
                    .expect("body glyph")
                    .pos
                    .y;
                let code_y = row
                    .glyphs
                    .iter()
                    .find(|g| g.chr == '-')
                    .expect("code glyph")
                    .pos
                    .y;
                assert!(
                    (code_y - body_y).abs() < 0.5,
                    "inline-code baseline shifted by {} px",
                    code_y - body_y
                );

                // An all-code line still gets the body row height via a
                // zero-width strut.
                let only = inline_job(ui, "`irun-app`", Color32::WHITE);
                assert!(only.text.ends_with('\u{200b}'));
            });
        });
    }
}

fn card_ui(ui: &mut Ui, card: &mut Card, toggles: &mut Vec<usize>, idx: usize) {
    let colour = match card.status {
        CardStatus::Running => theme::WARNING(),
        CardStatus::Ok => theme::SUCCESS(),
        CardStatus::Err => Color32::from_rgb(196, 84, 76),
    };
    Frame::NONE
        .fill(theme::BG_WINDOW())
        .stroke(egui::Stroke::new(1.0_f32, theme::BORDER()))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::symmetric(8, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // Controls on the right first, then the tool name on the
                // left. The summary gets its own line below: inside a nested
                // layout a truncating label drifts and wraps oddly (the
                // "sudo / linux test" floating-text bug), so it is drawn as
                // a plain full-width row instead.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let status = match card.status {
                        CardStatus::Running => "running…",
                        CardStatus::Ok => "done",
                        CardStatus::Err => "failed",
                    };
                    ui.label(RichText::new(status).size(9.5).color(colour));
                    if !card.result.is_empty() {
                        if ui
                            .add(egui::Button::new(
                                RichText::new(if card.expanded { "hide" } else { "show" })
                                    .size(9.5),
                            ).frame(false))
                            .clicked()
                        {
                            toggles.push(idx);
                        }
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(tglyph::BOLT)
                                .font(theme::icon(12.0))
                                .color(colour),
                        );
                        ui.label(
                            RichText::new(&card.tool)
                                .monospace()
                                .size(10.5)
                                .strong(),
                        );
                    });
                });
            });
            if !card.summary.is_empty() {
                // Indent past the bolt glyph so the summary lines up with the
                // tool name above it instead of hugging the card edge.
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    ui.add(
                        egui::Label::new(
                            RichText::new(clip(&card.summary, 220))
                                .size(10.0)
                                .color(theme::TEXT_DIM()),
                        )
                        .wrap_mode(egui::TextWrapMode::Truncate),
                    );
                });
            }
            if card.expanded && !card.result.is_empty() {
                ui.add_space(3.0);
                egui::Frame::NONE
                    .fill(theme::BG_PANEL())
                    .corner_radius(CornerRadius::same(4))
                    .inner_margin(Margin::symmetric(6, 4))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(clip(&card.result, 4000))
                                    .monospace()
                                    .size(9.5),
                            )
                            .wrap_mode(egui::TextWrapMode::Wrap),
                        );
                    });
            }
        });
}

/// Minimal markdown: fenced code, headings, bullets, **bold** and `inline`.
fn md_ui(ui: &mut Ui, text: &str, base: Color32) {
    let mut in_code = false;
    let mut code = String::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if in_code {
                code_frame(ui, &code);
                code.clear();
                in_code = false;
            } else {
                in_code = true;
            }
            continue;
        }
        if in_code {
            code.push_str(line);
            code.push('\n');
            continue;
        }
        let t = line.trim_end();
        if t.is_empty() {
            ui.add_space(3.0);
            continue;
        }
        if let Some(h) = t.strip_prefix("# ") {
            ui.label(heading(h, 14.5, base));
        } else if let Some(h) = t.strip_prefix("## ") {
            ui.label(heading(h, 13.5, base));
        } else if let Some(h) = t.strip_prefix("### ") {
            ui.label(heading(h, 12.5, base));
        } else if let Some(item) = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("* "))
            .or_else(|| t.strip_prefix("• "))
        {
            // Keep the bullet in the same run so it hugs the text.
            inline_ui(ui, &format!("• {item}"), base);
        } else {
            inline_ui(ui, t, base);
        }
    }
    if in_code && !code.is_empty() {
        code_frame(ui, &code);
    }
}

fn heading(s: &str, size: f32, base: Color32) -> RichText {
    RichText::new(s.trim()).strong().size(size).color(base)
}

fn code_frame(ui: &mut Ui, code: &str) {
    egui::Frame::NONE
        .fill(theme::BG_PANEL())
        .corner_radius(CornerRadius::same(4))
        .inner_margin(Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(code.trim_end())
                        .monospace()
                        .size(10.5)
                        .color(theme::TEXT()),
                )
                .wrap_mode(egui::TextWrapMode::Wrap),
            );
        });
}

/// `**bold**` + `` `code` `` inside one wrapped line.
///
/// Everything goes into a single `LayoutJob` instead of a row of labels: egui
/// vertically centres each label in a wrapped row, so a smaller inline-code
/// label used to float above the surrounding sentence. One job keeps every run
/// on the same baseline and wraps the whole line naturally.
fn inline_ui(ui: &mut Ui, s: &str, base: Color32) {
    let job = inline_job(ui, s, base);
    if !job.sections.is_empty() {
        ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Wrap));
    }
}

fn inline_job(ui: &Ui, s: &str, base: Color32) -> egui::text::LayoutJob {
    let body = egui::FontId::new(11.5, egui::FontFamily::Proportional);
    let mono = egui::FontId::new(11.5, egui::FontFamily::Monospace);
    let (body_ascent, body_height, code_ascent) = ui.fonts(|f| {
        let metrics = |font: &egui::FontId| {
            let galley =
                f.layout_no_wrap("M".to_owned(), font.clone(), Color32::WHITE);
            galley
                .rows
                .first()
                .and_then(|row| row.glyphs.first())
                .map(|g| (g.font_ascent, g.font_height))
                .unwrap_or((0.0, 0.0))
        };
        let (body_ascent, body_height) = metrics(&body);
        let (code_ascent, _) = metrics(&mono);
        (body_ascent, body_height, code_ascent)
    });

    // Different fonts have different ascent/descent metrics. With egui's
    // default bottom alignment a smaller-metric monospace run sits a couple of
    // pixels above the sentence, which is what made inline code "float".
    // Give the code run a line height whose bottom edge shares the body
    // baseline, then keep the row tall enough with a body strut below.
    let body_height = body_height.max(1.0);
    let code_line_height = (code_ascent + (body_height - body_ascent)).max(1.0);
    let body_fmt = egui::TextFormat {
        font_id: body,
        color: base,
        line_height: Some(body_height),
        ..Default::default()
    };
    let code_fmt = egui::TextFormat {
        font_id: mono,
        color: base,
        background: theme::BG_PANEL(),
        expand_bg: 2.0,
        line_height: Some(code_line_height),
        ..Default::default()
    };

    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = ui.available_width().max(1.0);
    let mut has_body = false;

    // Split on backticks; odd segments are inline code.
    let mut code_mode = false;
    for seg in s.split('`') {
        if !seg.is_empty() {
            if code_mode {
                job.append(seg, 0.0, code_fmt.clone());
            } else {
                // The bundled font has no bold face; egui's `strong()` only
                // changes the colour, and the chat already uses that colour.
                let plain = seg.replace("**", "");
                if !plain.is_empty() {
                    job.append(&plain, 0.0, body_fmt.clone());
                    has_body = true;
                }
            }
        }
        code_mode = !code_mode;
    }

    if !has_body && !job.sections.is_empty() {
        // A zero-width body run supplies the normal row height/baseline when
        // the whole line is inline code.
        job.append("\u{200b}", 0.0, body_fmt);
    }
    job
}
