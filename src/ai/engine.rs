//! The Copilot engine task: conversation state, provider round-trips, tool
//! execution, approval pauses and the security gate.
//!
//! Runs on the tokio runtime; talks to the UI exclusively through
//! [`AiCmd`]/[`AiEvent`] channels. It never touches the app's state directly:
//! UI work is requested as [`UiAction`]s and answered back via
//! [`AiCmd::ActionDone`].

use crate::ai::config::{AiConfig, SecMode};
use crate::ai::provider::{Msg, Provider, ToolCall};
use crate::ai::security::{redact_text, scan_text, Risk};
use crate::ai::tools::{self, Executor, Prepared};
use crate::ai::{push_audit, SharedAudit, UiAction, UiContext};
use crate::log::{LogEntry, LogLevel, SharedLogs};
use eframe::egui;
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Messages the panel/app send into the engine task.
pub enum AiCmd {
    /// A new user message (starts a turn).
    UserMessage(String),
    /// Approval card: execute the pending call (optionally with an edited
    /// command line).
    Approve { id: u64, edited: Option<String> },
    /// Approval card: decline — the model is told and may continue.
    Reject { id: u64 },
    /// Stop the current turn entirely.
    Cancel,
    /// Wipe the conversation.
    NewChat,
    /// Replace the active configuration (saved or not).
    SetConfig(Box<AiConfig>),
    /// Probe a provider configuration (may be unsaved) and report models.
    TestConnection(Box<AiConfig>),
    /// Fetch the model list for a provider configuration.
    FetchModels(Box<AiConfig>),
    /// The UI finished executing a [`UiAction`].
    ActionDone { id: u64, result: String },
}

/// Events the engine emits for the panel to render.
pub enum AiEvent {
    /// A turn started (spinner on).
    TurnStart,
    /// Streamed assistant text (providers that support SSE).
    Delta(String),
    /// Final assistant text for this round.
    AssistantText(String),
    /// A tool call started executing.
    ToolStart { tool: String, summary: String },
    /// A tool call finished (`text` is the result the model saw).
    ToolResult {
        tool: String,
        summary: String,
        ok: bool,
        text: String,
    },
    /// Actor needs the user's OK before executing.
    Approval {
        id: u64,
        tool: String,
        summary: String,
        command: Option<String>,
        risk: Risk,
        reason: String,
    },
    /// Sensitive content found — choose how to send it.
    SensitiveGate {
        id: u64,
        source: String,
        hits: Vec<String>,
        original_preview: String,
        redacted_preview: String,
    },
    /// The engine needs a UI action executed by the app thread.
    Action {
        id: u64,
        action: UiAction,
        label: String,
    },
    /// Plain-text note for the chat.
    Info(String),
    /// Error note for the chat.
    Error(String),
    /// Turn finished (busy off).
    TurnEnd,
    /// Model list for the settings modal (list, note — note doubles as error).
    Models(Vec<String>, String),
    /// Test-connection outcome.
    TestResult(bool, String),
}

pub struct Engine {
    cfg: AiConfig,
    provider: Provider,
    convo: Vec<Msg>,
    deferred: VecDeque<String>,
    snapshot: Arc<RwLock<UiContext>>,
    logs: SharedLogs,
    audit: SharedAudit,
    exec: Executor,
    cmd_rx: UnboundedReceiver<AiCmd>,
    evt_tx: UnboundedSender<AiEvent>,
    ctx: egui::Context,
    in_turn: bool,
    /// Monotonic ids for approval/gate/action round-trips.
    next_rt_id: u64,
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: AiConfig,
        snapshot: Arc<RwLock<UiContext>>,
        logs: SharedLogs,
        audit: SharedAudit,
        cmd_rx: UnboundedReceiver<AiCmd>,
        evt_tx: UnboundedSender<AiEvent>,
        ctx: egui::Context,
    ) -> Self {
        let provider = Provider::new(&cfg);
        Self {
            cfg,
            provider,
            convo: Vec::new(),
            deferred: VecDeque::new(),
            snapshot,
            logs,
            audit,
            exec: Executor::new(),
            cmd_rx,
            evt_tx,
            ctx,
            in_turn: false,
            next_rt_id: 1,
        }
    }

    fn emit(&self, ev: AiEvent) {
        let _ = self.evt_tx.send(ev);
        self.ctx.request_repaint();
    }

    fn context(&self) -> UiContext {
        self.snapshot
            .read()
            .map(|c| c.clone())
            .unwrap_or_default()
    }

    pub async fn run(mut self) {
        while let Some(cmd) = self.cmd_rx.recv().await {
            self.handle(cmd).await;
            // Messages typed while an approval was pending run after it.
            while let Some(text) = self.deferred.pop_front() {
                self.run_turn(text).await;
            }
        }
    }

    async fn handle(&mut self, cmd: AiCmd) {
        match cmd {
            AiCmd::UserMessage(text) => self.run_turn(text).await,
            AiCmd::SetConfig(cfg) => {
                self.cfg = *cfg;
                self.provider = Provider::new(&self.cfg);
            }
            AiCmd::TestConnection(cfg) => {
                let p = Provider::new(&cfg);
                match p.list_models().await {
                    Ok(models) => self.emit(AiEvent::TestResult(
                        true,
                        format!("Connected — {} models available.", models.len()),
                    )),
                    Err(e) => self.emit(AiEvent::TestResult(false, e)),
                }
            }
            AiCmd::FetchModels(cfg) => {
                let p = Provider::new(&cfg);
                match p.list_models().await {
                    Ok(models) => {
                        let note = format!("{} models", models.len());
                        self.emit(AiEvent::Models(models, note));
                    }
                    Err(e) => self.emit(AiEvent::Models(Vec::new(), e)),
                }
            }
            AiCmd::NewChat => {
                self.convo.clear();
                self.emit(AiEvent::Info("Started a new conversation.".into()));
            }
            AiCmd::Cancel => {
                // Only meaningful while a turn is parked on an approval/gate;
                // those waits consume the command themselves.
            }
            AiCmd::Approve { .. } | AiCmd::Reject { .. } | AiCmd::ActionDone { .. } => {}
        }
    }

    // -----------------------------------------------------------------------
    // Turn loop
    // -----------------------------------------------------------------------

    async fn run_turn(&mut self, user_text: String) {
        if self.in_turn {
            self.deferred.push_back(user_text);
            return;
        }
        self.in_turn = true;
        self.emit(AiEvent::TurnStart);
        push_audit(
            &self.audit,
            "request",
            format!(
                "[{} · {}] {}",
                self.provider.flavor.label(),
                self.cfg.model,
                truncate(&user_text, 400)
            ),
        );
        self.convo.push(Msg::user(user_text));

        let max_rounds = 8usize;
        for _round in 0..max_rounds {
            let sys = self.system_prompt();
            let mut msgs = Vec::with_capacity(self.convo.len() + 1);
            msgs.push(Msg::system(sys));
            msgs.extend(self.convo.iter().cloned());

            let model = self.cfg.model.clone();
            let evt = self.evt_tx.clone();
            let ectx = self.ctx.clone();
            let mut on_delta = move |d: &str| {
                let _ = evt.send(AiEvent::Delta(d.to_string()));
                ectx.request_repaint();
            };
            let reply = self
                .provider
                .chat(
                    &model,
                    &msgs,
                    &tools::defs(),
                    self.cfg.temperature,
                    self.cfg.streaming,
                    &mut on_delta,
                )
                .await;
            let reply = match reply {
                Ok(r) => r,
                Err(e) => {
                    push_audit(&self.audit, "error", format!("provider: {e}"));
                    self.emit(AiEvent::Error(format!("Provider error: {e}")));
                    break;
                }
            };

            let text = reply.text.trim().to_string();
            if !text.is_empty() {
                self.emit(AiEvent::AssistantText(text.clone()));
                push_audit(&self.audit, "info", format!("assistant: {}", truncate(&text, 300)));
            }
            if reply.tool_calls.is_empty() {
                if !text.is_empty() {
                    self.convo.push(Msg::assistant(text, Vec::new()));
                }
                break;
            }

            self.convo
                .push(Msg::assistant(text, reply.tool_calls.clone()));
            let mut cancelled = false;
            for (i, call) in reply.tool_calls.iter().enumerate() {
                match self.run_call(call).await {
                    Ok(result) => {
                        self.convo
                            .push(Msg::tool_result(&call.id, &call.name, result));
                    }
                    Err(CancelReason::Stopped) => {
                        // Keep the transcript valid for strict providers: every
                        // tool call must have a result before the next turn.
                        for rest in &reply.tool_calls[i..] {
                            self.convo.push(Msg::tool_result(
                                &rest.id,
                                &rest.name,
                                "Cancelled by the user before execution.",
                            ));
                        }
                        cancelled = true;
                        break;
                    }
                }
            }
            if cancelled {
                self.emit(AiEvent::Info("Stopped at your request.".into()));
                break;
            }
        }

        self.in_turn = false;
        self.emit(AiEvent::TurnEnd);
    }

    // -----------------------------------------------------------------------
    // One tool call: validate → approve → execute → security-filter
    // -----------------------------------------------------------------------

    async fn run_call(&mut self, call: &ToolCall) -> Result<String, CancelReason> {
        let ctx = self.context();
        let args: serde_json::Value =
            serde_json::from_str(&call.args).unwrap_or_else(|_| serde_json::json!({}));
        let mut prepared = match tools::prepare(&call.name, &args, &ctx) {
            Ok(p) => p,
            Err(e) => {
                self.emit(AiEvent::ToolResult {
                    tool: call.name.clone(),
                    summary: String::new(),
                    ok: false,
                    text: e.clone(),
                });
                return Ok(format!("error: {e}"));
            }
        };

        // -- Approval ------------------------------------------------------
        let auto_ok = self.cfg.auto_safe && prepared.risk == Risk::Safe;
        if !auto_ok {
            let id = self.next_rt_id;
            self.next_rt_id += 1;
            self.emit(AiEvent::Approval {
                id,
                tool: prepared.tool.clone(),
                summary: prepared.summary.clone(),
                command: prepared.command.clone(),
                risk: prepared.risk,
                reason: prepared.reason.clone(),
            });
            loop {
                match self.cmd_rx.recv().await {
                    Some(AiCmd::Approve { id: got, edited }) => {
                        if got != id {
                            continue;
                        }
                        if let Some(cmd) = edited {
                            if let Prepared::Ssh { command, .. } = &mut prepared.prepared {
                                *command = cmd;
                            }
                        }
                        push_audit(
                            &self.audit,
                            "approval",
                            format!("granted: {} [{}]", prepared.summary, prepared.risk.label()),
                        );
                        break;
                    }
                    Some(AiCmd::Reject { id: got }) => {
                        if got != id {
                            continue;
                        }
                        push_audit(&self.audit, "approval", format!("declined: {}", prepared.summary));
                        return Ok("The user declined this action. Ask how they want to proceed instead.".to_string());
                    }
                    Some(AiCmd::Cancel) => {
                        push_audit(&self.audit, "approval", "cancelled turn".to_string());
                        return Err(CancelReason::Stopped);
                    }
                    Some(AiCmd::UserMessage(t)) => self.deferred.push_back(t),
                    Some(AiCmd::SetConfig(cfg)) => {
                        self.cfg = *cfg;
                        self.provider = Provider::new(&self.cfg);
                    }
                    Some(_) => {}
                    None => return Err(CancelReason::Stopped),
                }
            }
        }

        // -- Execution ------------------------------------------------------
        self.emit(AiEvent::ToolStart {
            tool: prepared.tool.clone(),
            summary: prepared.summary.clone(),
        });

        let (raw, ok) = match &prepared.prepared {
            Prepared::State(q) => (tools::state_answer(*q, &ctx), true),
            Prepared::Ssh {
                session_id,
                label,
                command,
                in_terminal: true,
            } => {
                // Interactive work goes through the UI: the command lands in
                // the visible shell tab (opened if needed), where the user can
                // answer sudo/password prompts and watch it run.
                let rid = self.next_rt_id;
                self.next_rt_id += 1;
                self.emit(AiEvent::Action {
                    id: rid,
                    action: UiAction::RunInTerminal {
                        session_id: session_id.clone(),
                        label: label.clone(),
                        command: command.clone(),
                    },
                    label: format!("run in the terminal on {label}"),
                });
                let result = self.await_action(rid).await?;
                let ok = result.starts_with("[exit 0]");
                crate::log::push_log(
                    &self.logs,
                    LogEntry::new(
                        LogLevel::Info,
                        "AI",
                        &format!("{label} · terminal: {command}"),
                    ),
                );
                (result, ok)
            }
            Prepared::Action { action, label } => {
                let id = self.next_rt_id;
                self.next_rt_id += 1;
                self.emit(AiEvent::Action {
                    id,
                    action: action.clone(),
                    label: label.clone(),
                });
                let result = self.await_action(id).await?;
                let ok = !result.to_lowercase().contains("cannot")
                    && !result.to_lowercase().contains("could not")
                    && !result.to_lowercase().contains("not showing")
                    && !result.to_lowercase().contains("no open window");
                (result, ok)
            }
            _ => {
                let session_id = prepared_session_id(&prepared.prepared).to_string();
                let label = match &prepared.prepared {
                    Prepared::Ssh { label, .. }
                    | Prepared::ListDir { label, .. }
                    | Prepared::ReadFile { label, .. }
                    | Prepared::Download { label, .. }
                    | Prepared::Upload { label, .. } => label.clone(),
                    _ => String::new(),
                };
                let res = match ctx.profiles.iter().find(|p| p.id == session_id).cloned() {
                    Some(profile) => self.exec.run(&profile, &prepared.prepared, &self.logs).await,
                    None => Err(format!("unknown session {session_id}")),
                };
                match res {
                    Ok(text) => {
                        if let Some(cmd) = &prepared.command {
                            crate::log::push_log(
                                &self.logs,
                                LogEntry::new(
                                    LogLevel::Info,
                                    "AI",
                                    &format!("{label}: {cmd}"),
                                ),
                            );
                        }
                        (text, true)
                    }
                    Err(e) => (format!("error: {e}"), false),
                }
            }
        };

        // -- Security filter -------------------------------------------------
        let mut outcome_note: Option<String> = None;
        let processed = match self.security_filter(&prepared, &raw).await? {
            FilterResult::Pass(text) => text,
            FilterResult::Outcome { text, note } => {
                outcome_note = Some(note);
                text
            }
        };

        push_audit(
            &self.audit,
            "tool",
            format!(
                "{} | {} | {} | {}",
                prepared.tool,
                prepared.summary,
                if ok { "ok" } else { "err" },
                truncate(&processed.trim(), 200)
            ),
        );
        let mut card_text = clip_result(&processed);
        if let Some(note) = outcome_note {
            card_text = format!("{note}\n{card_text}");
        }
        self.emit(AiEvent::ToolResult {
            tool: prepared.tool.clone(),
            summary: prepared.summary.clone(),
            ok,
            text: card_text,
        });
        Ok(processed)
    }

    async fn await_action(&mut self, id: u64) -> Result<String, CancelReason> {
        loop {
            match self.cmd_rx.recv().await {
                Some(AiCmd::ActionDone { id: got, result }) => {
                    if got == id {
                        return Ok(result);
                    }
                }
                Some(AiCmd::UserMessage(t)) => self.deferred.push_back(t),
                Some(AiCmd::Cancel) => return Err(CancelReason::Stopped),
                Some(_) => {}
                None => return Err(CancelReason::Stopped),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Security filter
    // -----------------------------------------------------------------------

    async fn security_filter(
        &mut self,
        prepared: &tools::Call,
        raw: &str,
    ) -> Result<FilterResult, CancelReason> {
        let is_local = self.provider.is_local();
        let is_file_content = matches!(prepared.prepared, Prepared::ReadFile { .. });
        let term = if is_file_content { "file" } else { "output" };

        if is_local {
            // Local model: data stays on this machine — no redaction/gates.
            return Ok(FilterResult::Pass(raw.to_string()));
        }

        match self.cfg.security {
            SecMode::Strict => {
                if is_file_content {
                    return Ok(FilterResult::Outcome {
                        text: "[NOT SENT] STRICT mode: file contents are not shared with the model. Ask the user to describe what they need, or use execute_command (redacted) instead."
                            .to_string(),
                        note: "[strict mode] file content withheld from the model".to_string(),
                    });
                }
                let (red, n) = redact_text(raw);
                if n > 0 {
                    return Ok(FilterResult::Outcome {
                        text: format!("{red}\n[{n} sensitive value(s) redacted by Culvert STRICT mode]"),
                        note: format!("[strict mode] {n} sensitive value(s) redacted"),
                    });
                }
                Ok(FilterResult::Pass(raw.to_string()))
            }
            SecMode::Balanced => {
                let hits = scan_text(raw);
                let source_sensitive = prepared
                    .sensitive
                    .as_deref()
                    .map(|r| format!("{} (file: {})", r, prepared.summary));
                if hits.is_empty() && source_sensitive.is_none() {
                    return Ok(FilterResult::Pass(raw.to_string()));
                }
                let (red, n) = redact_text(raw);
                let hit_desc: Vec<String> = hits
                    .iter()
                    .map(|h| format!("{} ×{} · {}", h.kind, h.count, h.sample))
                    .collect();
                let source = source_sensitive.unwrap_or_else(|| {
                    format!("{n} sensitive value(s) found in the {term}")
                });
                // The tool card always shows the note; the model gets either
                // the redacted text or the original per the user's choice.
                let model_text = match self
                    .sensitive_decision(prepared, &source, &hit_desc, raw, &red)
                    .await?
                {
                    SendChoice::Original => raw.to_string(),
                    SendChoice::Redacted => red,
                    SendChoice::Cancel => {
                        return Ok(FilterResult::Outcome {
                            text: "The user declined to share this content with the model.".to_string(),
                            note: "sharing cancelled by the user".to_string(),
                        });
                    }
                };
                Ok(FilterResult::Pass(model_text))
            }
            SecMode::Full => {
                let hits = scan_text(raw);
                if hits.is_empty() {
                    return Ok(FilterResult::Pass(raw.to_string()));
                }
                let (red, n) = redact_text(raw);
                let hit_desc: Vec<String> = hits
                    .iter()
                    .map(|h| format!("{} ×{} · {}", h.kind, h.count, h.sample))
                    .collect();
                let source = format!("{n} sensitive value(s) found in the {term}");
                let model_text = match self
                    .sensitive_decision(prepared, &source, &hit_desc, raw, &red)
                    .await?
                {
                    SendChoice::Original => raw.to_string(),
                    SendChoice::Redacted => red,
                    SendChoice::Cancel => {
                        return Ok(FilterResult::Outcome {
                            text: "The user declined to share this content with the model.".to_string(),
                            note: "sharing cancelled by the user".to_string(),
                        });
                    }
                };
                Ok(FilterResult::Pass(model_text))
            }
        }
    }

    async fn sensitive_decision(
        &mut self,
        prepared: &tools::Call,
        source: &str,
        hits: &[String],
        original: &str,
        redacted: &str,
    ) -> Result<SendChoice, CancelReason> {
        let id = self.next_rt_id;
        self.next_rt_id += 1;
        push_audit(
            &self.audit,
            "sensitive",
            format!("gate: {} | {source}", prepared.summary),
        );
        self.emit(AiEvent::SensitiveGate {
            id,
            source: source.to_string(),
            hits: hits.to_vec(),
            original_preview: preview(original),
            redacted_preview: preview(redacted),
        });
        loop {
            match self.cmd_rx.recv().await {
                Some(AiCmd::Approve { id: got, edited }) => {
                    if got != id {
                        continue;
                    }
                    // For the gate, Approve = send original.
                    let choice = if edited.as_deref() == Some("redacted") {
                        SendChoice::Redacted
                    } else {
                        SendChoice::Original
                    };
                    push_audit(
                        &self.audit,
                        "sensitive",
                        format!(
                            "user chose {}",
                            match choice {
                                SendChoice::Original => "send original",
                                SendChoice::Redacted => "send redacted",
                                SendChoice::Cancel => "cancel",
                            }
                        ),
                    );
                    return Ok(choice);
                }
                Some(AiCmd::Reject { id: got }) => {
                    if got != id {
                        continue;
                    }
                    push_audit(&self.audit, "sensitive", "user cancelled sharing".to_string());
                    return Ok(SendChoice::Cancel);
                }
                Some(AiCmd::Cancel) => return Err(CancelReason::Stopped),
                Some(AiCmd::UserMessage(t)) => self.deferred.push_back(t),
                Some(_) => {}
                None => return Err(CancelReason::Stopped),
            }
        }
    }

    // -----------------------------------------------------------------------
    // System prompt
    // -----------------------------------------------------------------------

    fn system_prompt(&self) -> String {
        let ctx = self.context();
        let mut p = String::from(
            "You are the AI Copilot inside Culvert, a native SSH client and tunnel manager on the user's Windows desktop. You control Culvert through the provided tools.\n\n\
             Rules:\n\
             - Every claim about a server must come from a tool result. Never invent output.\n\
             - The app executes tools; some ask the user for approval first. If the user declines, adapt.\n\
             - Prefer the active session and current directory when the user is vague.\n\
             - Use bounded, non-interactive commands (grep ... | tail -n 200). Never use top/tail -f/watch/less.\n\
             - Keep answers short: what you found, the exact command(s) used in `backticks`, and the next useful step.\n\
             - Mention when output was truncated or values were [REDACTED] by Culvert's security layer.\n\
             - For state-changing work, call the tool once — the user will see an approval card.\n\
             - sudo is usable: run it normally when a task needs root (Culvert pipes the saved profile password into sudo -S when required; passwordless sudo works too). Never ask the user for a password.\n\
             - For installs and other interactive work (package managers, prompts, anything needing root where sudo may ask), call execute_command with in_terminal=true: the command is typed into the visible terminal tab, the user can answer prompts there, and the app returns what the screen showed. Output may be only the visible tail. State-changing commands do this by default; read-only ones use the hidden channel.\n\nContext:\n",
        );
        match ctx
            .active_session_id
            .as_ref()
            .and_then(|id| ctx.profiles_summary.iter().find(|s| &s.id == id))
        {
            Some(s) => p.push_str(&format!(
                "- Active session: {} ({}@{}){}\n",
                s.name,
                s.username,
                s.host,
                s.group
                    .as_ref()
                    .map(|g| format!(" [group {g}]"))
                    .unwrap_or_default()
            )),
            None => p.push_str("- Active session: none\n"),
        }
        match &ctx.dock {
            Some(d) => {
                let s = ctx
                    .profiles_summary
                    .iter()
                    .find(|p| p.id == d.session_id)
                    .map(|p| p.name.as_str())
                    .unwrap_or("?");
                p.push_str(&format!(
                    "- File browser: showing {s}, current directory {}, selected {}\n",
                    d.cwd,
                    d.selected.as_deref().unwrap_or("nothing")
                ));
            }
            None => p.push_str("- File browser: not open\n"),
        }
        let open: Vec<&str> = ctx
            .profiles_summary
            .iter()
            .filter(|s| ctx.open_session_ids.contains(&s.id))
            .map(|s| s.name.as_str())
            .collect();
        p.push_str(&format!(
            "- Open shell windows: {}\n",
            if open.is_empty() {
                "none".to_string()
            } else {
                open.join(", ")
            }
        ));
        if ctx.profiles_summary.is_empty() {
            p.push_str("- Saved profiles: none\n");
        } else {
            p.push_str("- Saved profiles:\n");
            for s in ctx.profiles_summary.iter().take(40) {
                p.push_str(&format!(
                    "  · {} ({}@{}){}\n",
                    s.name,
                    s.username,
                    s.host,
                    s.group
                        .as_ref()
                        .map(|g| format!(" [group {g}]"))
                        .unwrap_or_default()
                ));
            }
        }
        if ctx.tunnels.is_empty() {
            p.push_str("- Tunnels: none configured\n");
        } else {
            p.push_str("- Tunnels:\n");
            for t in ctx.tunnels.iter().take(40) {
                p.push_str(&format!(
                    "  · [{}] {} — via '{}' (key {})\n",
                    if t.running { "running" } else { "stopped" },
                    t.summary,
                    t.session_label,
                    t.key
                ));
            }
        }
        if self.provider.is_local() {
            p.push_str("- Security: LOCAL model — data stays on this machine.\n");
        } else {
            p.push_str(&format!(
                "- Security mode: {} — sensitive values may appear as [REDACTED].\n",
                self.cfg.security.label()
            ));
        }
        p
    }
}

#[derive(Clone, Copy)]
enum SendChoice {
    Original,
    Redacted,
    Cancel,
}

enum FilterResult {
    Pass(String),
    /// Result replaced (blocked or noted) — `note` goes to the tool card.
    Outcome { text: String, note: String },
}

enum CancelReason {
    Stopped,
}

fn prepared_session_id(p: &Prepared) -> &str {
    match p {
        Prepared::Ssh { session_id, .. }
        | Prepared::ListDir { session_id, .. }
        | Prepared::ReadFile { session_id, .. }
        | Prepared::Download { session_id, .. }
        | Prepared::Upload { session_id, .. } => session_id,
        _ => "",
    }
}

fn truncate(s: &str, n: usize) -> String {
    let t = s.trim();
    if t.chars().count() > n {
        let cut: String = t.chars().take(n).collect();
        format!("{cut}…")
    } else {
        t.to_string()
    }
}

fn preview(s: &str) -> String {
    truncate(s, 1500)
}

fn clip_result(s: &str) -> String {
    truncate(s, 6000)
}
