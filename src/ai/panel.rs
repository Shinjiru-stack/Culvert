//! The AI Copilot panel: chat surface, approval cards, security gates,
//! execution/audit history and the provider settings modal.
//!
//! All UI state lives here; the engine task is reached only through the
//! command/event channels created in [`AiPanel::new`].

use crate::ai::config::{self, AiConfig, SecMode};
use crate::ai::engine::{AiCmd, AiEvent};
use crate::ai::provider::Flavor;
use crate::ai::security::Risk;
use crate::ai::{new_context, new_audit, push_audit, AuditEntry, SharedAudit, UiAction, UiContext};
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

    msgs: Vec<UiMsg>,
    streaming: Option<String>,
    input: String,
    pending: Option<Pending>,
    edit_command: bool,
    edit_buf: String,
    tab: Tab,
    records: Vec<ExecRecord>,
    pending_action: Option<(u64, UiAction, String)>,

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
            msgs: Vec::new(),
            streaming: None,
            input: String::new(),
            pending: None,
            edit_command: false,
            edit_buf: String::new(),
            tab: Tab::Chat,
            records: Vec::new(),
            pending_action: None,
            cmd_tx,
            evt_rx,
            engine_started: false,
            rt,
            logs,
        };
        me.msgs.push(UiMsg {
            role: Role::Info,
            text: "Ask about your servers in plain language — e.g. “check disk space on the demo host”, “show running containers”, “find log files larger than 1 GB”.\n\nI run read-only commands straight away; anything that changes state asks for your approval first.".into(),
            card: None,
        });
        me
    }

    /// True when no model is configured yet (panel should open settings).
    pub fn needs_setup(&self) -> bool {
        self.cfg.model.trim().is_empty()
    }

    pub fn open_settings(&mut self) {
        self.draft = self.cfg.clone();
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
        self.send(AiCmd::UserMessage(text));
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
                } => {
                    self.edit_command = false;
                    self.edit_buf = command.clone().unwrap_or_default();
                    self.pending = Some(Pending {
                        id,
                        kind: PendingKind::Approval {
                            tool,
                            summary,
                            command,
                            risk,
                            reason,
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
                    self.models_note = Some((ok, note));
                }
                AiEvent::TestResult(ok, msg) => {
                    self.test_note = Some((ok, msg));
                }
            }
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

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| match self.tab {
                Tab::Chat => self.chat_body(ui),
                Tab::History => self.history_body(ui),
            });
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
                            ui.add_space(5.0);
                            ui.horizontal(|ui| {
                                let exec = if self.edit_command {
                                    "Execute edited"
                                } else {
                                    "Execute"
                                };
                                if ui
                                    .add(egui::Button::new(
                                        RichText::new(exec).size(11.5).color(Color32::WHITE),
                                    ).fill(theme::SUCCESS()))
                                    .clicked()
                                {
                                    let edited = if self.edit_command
                                        && !self.edit_buf.trim().is_empty()
                                    {
                                        Some(self.edit_buf.clone())
                                    } else {
                                        None
                                    };
                                    act = Some(UiCmd::Approve { id: p.id, edited });
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
            match act {
                UiCmd::Approve { id, edited } => self.send(AiCmd::Approve { id, edited }),
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
            "Ask anything — “check disk space”, “tail the nginx errors”, “start the web tunnel”…"
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
                                    }
                                }
                            });
                    });
                    ui.add_space(4.0);

                    ui.label(RichText::new("API key").size(11.5).strong());
                    ui.add(
                        egui::TextEdit::singleline(&mut self.draft.api_key)
                            .password(true)
                            .desired_width(f32::INFINITY)
                            .hint_text(flavor.key_hint()),
                    );
                    ui.label(
                        RichText::new("Encrypted with Windows DPAPI (your Windows account is the key).")
                            .size(9.5)
                            .color(theme::TEXT_FAINT()),
                    );
                    ui.add_space(6.0);

                    ui.label(RichText::new("Base URL").size(11.5).strong());
                    ui.add(
                        egui::TextEdit::singleline(&mut self.draft.base_url)
                            .desired_width(f32::INFINITY)
                            .hint_text(flavor.default_base()),
                    );
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
                            self.settings_open = false;
                        }
                    });
                });
            });
        if !open {
            self.settings_open = false;
        }
        saved
    }
}

enum UiCmd {
    Approve { id: u64, edited: Option<String> },
    Reject { id: u64 },
    CancelTurn,
}

// ---------------------------------------------------------------------------
// Small UI helpers
// ---------------------------------------------------------------------------

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
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("• ").color(base));
                inline_ui(ui, item, base);
            });
        } else {
            ui.horizontal_wrapped(|ui| inline_ui(ui, t, base));
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
fn inline_ui(ui: &mut Ui, s: &str, base: Color32) {
    // First split on backticks; odd segments are inline code.
    let mut code_mode = false;
    let mut rest = s;
    loop {
        match rest.find('`') {
            Some(i) => {
                let seg = &rest[..i];
                if code_mode {
                    ui.label(
                        RichText::new(seg)
                            .monospace()
                            .size(10.5)
                            .background_color(theme::BG_PANEL())
                            .color(base),
                    );
                } else {
                    bold_ui(ui, seg, base);
                }
                code_mode = !code_mode;
                rest = &rest[i + 1..];
            }
            None => {
                if code_mode {
                    ui.label(RichText::new(rest).monospace().size(10.5).color(base));
                } else {
                    bold_ui(ui, rest, base);
                }
                break;
            }
        }
    }
}

fn bold_ui(ui: &mut Ui, s: &str, base: Color32) {
    let mut bold = false;
    let mut rest = s;
    loop {
        match rest.find("**") {
            Some(i) => {
                let seg = &rest[..i];
                if !seg.is_empty() {
                    let rt = RichText::new(seg).size(11.5).color(base);
                    ui.label(if bold { rt.strong() } else { rt });
                }
                bold = !bold;
                rest = &rest[i + 2..];
            }
            None => {
                if !rest.is_empty() {
                    let rt = RichText::new(rest).size(11.5).color(base);
                    ui.label(if bold { rt.strong() } else { rt });
                }
                break;
            }
        }
    }
}
