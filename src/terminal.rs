//! Interactive SSH shell in a tab: a real PTY session on the server,
//! rendered locally through a vt100 terminal emulator (colors, cursor,
//! scrollback, alternate screen), drawn with egui.

use crate::log::SharedLogs;
use crate::session::SshSession;
use crate::ssh;
use crate::theme;
use eframe::egui::{self, Align2, Color32, FontFamily, FontId, Rect, RichText, Stroke, StrokeKind};
use russh::ChannelMsg;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

const SCROLLBACK_LINES: usize = 4000;
/// Terminal canvas. MobaXterm keeps its shell black regardless of the app
/// chrome, so the canvas no longer follows the light/dark palette switch.
#[allow(non_snake_case)]
fn TERM_BG() -> Color32 {
    Color32::from_rgb(0, 0, 0)
}
/// Terminal foreground: the classic xterm light grey used by MobaXterm.
#[allow(non_snake_case)]
fn TERM_FG() -> Color32 {
    Color32::from_rgb(229, 229, 229) // #E5E5E5
}

// ---------------------------------------------------------------------------
// Messages between UI thread and the async shell task
// ---------------------------------------------------------------------------

enum TermCmd {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Close,
}

enum TermEvent {
    Output(Vec<u8>),
    Connected,
    Closed(String),
    /// Fresh server-health snapshot for the monitor strip (None = unreachable).
    Mon(Option<ServerData>),
}

/// Monitor control: the strip keeper runs its own low-rate SSH connection per
/// tab and stops when the tab closes/reconnects.
enum MonCmd {
    Stop,
}

/// Non-interactive server-health snapshot taken through a dedicated exec
/// channel every 10 s (MobaXterm-style status bar under the shell).
#[derive(Clone, Debug, Default)]
struct ServerData {
    user: String,
    host: String,
    /// One-minute load average.
    load: f32,
    cores: u32,
    mem_used: u64,
    mem_total: u64,
    disk_path: String,
    /// df "Capacity" percentage for the root filesystem.
    disk_pct: f32,
    disk_free: u64,
    uptime: String,
}

impl ServerData {
    fn mem_pct(&self) -> f32 {
        if self.mem_total == 0 {
            0.0
        } else {
            self.mem_used as f32 / self.mem_total as f32 * 100.0
        }
    }

    /// Compact "512M / 2.0G" memory readout.
    fn mem_text(&self) -> String {
        if self.mem_total == 0 {
            return "n/a".to_string();
        }
        format!("{} / {}", fmt_size(self.mem_used), fmt_size(self.mem_total))
    }

    /// Compact "<free> free" disk readout.
    fn disk_text(&self) -> String {
        if self.disk_free == 0 {
            return "n/a".to_string();
        }
        format!("{} free", fmt_size(self.disk_free))
    }
}

/// Human-size from bytes the df/meminfo way (top unit, one decimal for G+).
fn fmt_size(v: u64) -> String {
    if v >= 1024 * 1024 * 1024 * 1024 {
        format!("{:.2}T", v as f64 / (1024.0 * 1024.0 * 1024.0 * 1024.0))
    } else if v >= 1024 * 1024 * 1024 {
        format!("{:.1}G", v as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if v >= 1024 * 1024 {
        format!("{:.0}M", v as f64 / (1024.0 * 1024.0))
    } else if v >= 1024 {
        format!("{:.0}K", v as f64 / 1024.0)
    } else {
        format!("{v}B")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TermStatus {
    Connecting,
    Connected,
    Closed(String),
}

// ---------------------------------------------------------------------------
// Terminal session (one tab)
// ---------------------------------------------------------------------------

pub struct TerminalSession {
    pub session_id: String,
    pub title: String,
    pub status: TermStatus,
    /// Set by the overlay buttons; the app acts on them after drawing.
    pub want_reconnect: bool,
    pub want_close: bool,
    parser: vt100::Parser,
    cmd_tx: UnboundedSender<TermCmd>,
    evt_rx: UnboundedReceiver<TermEvent>,
    focus_id: egui::Id,
    session: SshSession,
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    logs: SharedLogs,
    cols: u16,
    rows: u16,
    /// Stop line for the server-monitor task (own per-tab SSH connection);
    /// replacing it on reconnect/close drops the sender and ends the task.
    mon_tx: Option<UnboundedSender<MonCmd>>,
    /// Latest monitor snapshot for the strip under the shell.
    mon: Option<ServerData>,
}

impl TerminalSession {
    pub fn spawn(
        ctx: &egui::Context,
        rt: tokio::runtime::Handle,
        logs: SharedLogs,
        session: SshSession,
    ) -> Self {
        let (cmd_tx, _) = tokio::sync::mpsc::unbounded_channel::<TermCmd>();
        let (_, evt_rx) = tokio::sync::mpsc::unbounded_channel::<TermEvent>();
        let mut me = Self {
            session_id: session.id.clone(),
            title: session.name.clone(),
            status: TermStatus::Connecting,
            want_reconnect: false,
            want_close: false,
            parser: vt100::Parser::new(30, 100, SCROLLBACK_LINES),
            cmd_tx,
            evt_rx,
            focus_id: egui::Id::new(uuid::Uuid::new_v4()),
            session,
            rt,
            ctx: ctx.clone(),
            logs,
            cols: 100,
            rows: 30,
            mon_tx: None,
            mon: None,
        };
        me.start_task();
        me
    }

    fn start_task(&mut self) {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (evt_tx, evt_rx) = tokio::sync::mpsc::unbounded_channel();
        self.cmd_tx = cmd_tx;
        self.evt_rx = evt_rx;
        let session = self.session.clone();
        let logs = self.logs.clone();
        let ctx = self.ctx.clone();
        let evt_tx_mon = evt_tx.clone();
        let (cols, rows) = (self.cols, self.rows);
        self.rt.spawn(async move {
            shell_task(session, logs, ctx, evt_tx, cmd_rx, cols, rows).await;
        });

        // Server-monitor task: its own persistent SSH connection, sampled
        // every 10 s and rendered in the strip under the shell. Replacing
        // `mon_tx` (reconnect/close) drops the queue and ends the task.
        let (mon_tx, mon_rx) = tokio::sync::mpsc::unbounded_channel::<MonCmd>();
        self.mon_tx = Some(mon_tx);
        self.mon = None;
        let session = self.session.clone();
        let logs = self.logs.clone();
        let ctx = self.ctx.clone();
        self.rt.spawn(async move {
            monitor_task(session, logs, ctx, evt_tx_mon, mon_rx).await;
        });
    }

    /// Restart the shell in place (after disconnect).
    pub fn reconnect(&mut self) {
        self.status = TermStatus::Connecting;
        self.want_reconnect = false;
        self.parser = vt100::Parser::new(self.rows.max(1), self.cols.max(1), SCROLLBACK_LINES);
        self.start_task();
    }

    pub fn close(&self) {
        let _ = self.cmd_tx.send(TermCmd::Close);
        if let Some(m) = &self.mon_tx {
            let _ = m.send(MonCmd::Stop);
        }
    }

    /// Raw bytes straight to the shell (macros, quick-bar paste).
    pub fn send_bytes(&self, bytes: Vec<u8>) {
        self.send(TermCmd::Write(bytes));
    }

    /// Visible screen text (quick-bar Copy). Scrollback is not included.
    pub fn visible_text(&self) -> String {
        self.parser.screen().contents()
    }

    /// Clipboard text into the shell, honoring bracketed-paste mode.
    pub fn paste_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let bracketed = self.parser.screen().bracketed_paste();
        let mut bytes = Vec::new();
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
        }
        bytes.extend_from_slice(text.as_bytes());
        if bracketed {
            bytes.extend_from_slice(b"\x1b[201~");
        }
        self.send(TermCmd::Write(bytes));
    }

    /// Wipe screen + scrollback but keep the connection up.
    pub fn clear_screen(&mut self) {
        self.parser = vt100::Parser::new(self.rows.max(1), self.cols.max(1), SCROLLBACK_LINES);
    }

    /// Drain async shell events into the parser. Returns true
    /// when repainting is needed for a reason other than cursor blink.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(ev) = self.evt_rx.try_recv() {
            match ev {
                TermEvent::Output(b) => {
                    self.parser.process(&b);
                    changed = true;
                }
                TermEvent::Connected => {
                    self.status = TermStatus::Connected;
                    changed = true;
                }
                TermEvent::Mon(data) => {
                    self.mon = data;
                    changed = true;
                }
                TermEvent::Closed(reason) => {
                    self.status = TermStatus::Closed(reason);
                    self.mon = None;
                    // The shell died: the monitor strip no longer has a home
                    // (its own connection would keep polling pointlessly).
                    if let Some(m) = self.mon_tx.take() {
                        let _ = m.send(MonCmd::Stop);
                    }
                    changed = true;
                }
            }
        }
        changed
    }

    pub fn connection_label(&self) -> String {
        self.session.connection_label()
    }

    pub fn is_focused(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|m| m.has_focus(self.focus_id))
    }

    fn send(&self, cmd: TermCmd) {
        let _ = self.cmd_tx.send(cmd);
    }

    // -----------------------------------------------------------------------
    // Rendering + input
    // -----------------------------------------------------------------------

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let rect = ui.available_rect_before_wrap();
        // Session tool strip: the session name and connection state (files
        // browsing lives in the sidebar Files tool, not in this window).
        let strip = Rect::from_min_size(rect.min, egui::vec2(rect.width(), 28.0));
        // MobaXterm-style server monitor bar when the shell is live.
        let mon_h = match self.status {
            TermStatus::Connected => 22.0,
            _ => 0.0,
        };
        let body = Rect::from_min_max(
            egui::pos2(rect.left(), strip.bottom()),
            egui::pos2(rect.right(), rect.bottom() - mon_h),
        );
        self.session_strip(ui, strip);
        self.shell_body(ui, body);
        if mon_h > 0.0 {
            let mon_rect = Rect::from_min_max(
                egui::pos2(rect.left(), body.bottom()),
                egui::pos2(rect.right(), rect.bottom()),
            );
            self.server_bar(ui, mon_rect);
        }
    }

    /// Slim server-health strip under the shell canvas: user, load, memory,
    /// root-disk usage and uptime, refreshed every 10 s over its own exec
    /// channel (MobaXterm look).
    fn server_bar(&self, ui: &mut egui::Ui, rect: Rect) {
        const BAR_BG: Color32 = Color32::from_rgb(20, 21, 23);
        const HAIRLINE: Color32 = Color32::from_rgb(52, 55, 59);
        const BAR_TEXT: Color32 = Color32::from_gray(206);
        const GOOD: Color32 = Color32::from_rgb(141, 206, 133);
        const WARN: Color32 = Color32::from_rgb(224, 166, 60);
        const BAD: Color32 = Color32::from_rgb(232, 116, 108);
        const ICON_COLOR: Color32 = Color32::from_rgb(132, 200, 240);

        let tint = |pct: f32, warn: f32, bad: f32| {
            if pct >= bad {
                BAD
            } else if pct >= warn {
                WARN
            } else {
                GOOD
            }
        };

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, BAR_BG);
        painter.line_segment(
            [
                egui::pos2(rect.left(), rect.top() + 0.5),
                egui::pos2(rect.right(), rect.top() + 0.5),
            ],
            Stroke::new(0.5_f32, HAIRLINE),
        );

        let Some(d) = &self.mon else {
            return;
        };
        let cy = rect.center().y;
        let mono = FontId::new(10.5, FontFamily::Monospace);
        let iconf = theme::icon(12.0);

        let mut x = rect.left() + 8.0;
        // helper-closure: draws glyph + text, advances x by the text width.
        let seg = |p: &egui::Painter,
                   icon_glyph: char,
                   icon_color: Color32,
                   text: &String,
                   text_color: Color32,
                   x: &mut f32,
                   gap: f32| {
            p.text(
                egui::pos2(*x, cy),
                Align2::LEFT_CENTER,
                icon_glyph.to_string(),
                iconf.clone(),
                icon_color,
            );
            *x += 16.0;
            p.text(egui::pos2(*x, cy), Align2::LEFT_CENTER, text.clone(), mono.clone(), text_color);
            *x += p
                .layout_no_wrap(text.clone(), mono.clone(), text_color)
                .rect
                .width()
                + gap;
        };

        seg(&painter, GLYPH_USER, ICON_COLOR, &format!("{}@{}", d.user, d.host), BAR_TEXT, &mut x, 16.0);

        let cpu_ptr = if d.cores > 0 { d.load / d.cores as f32 * 100.0 } else { 0.0 };
        let cpu_txt = if d.cores > 0 {
            format!("CPU {:.0}% ({:.2}/{n} cores)", cpu_ptr, d.load, n = d.cores)
        } else {
            format!("load {:.2}", d.load)
        };
        seg(&painter, GLYPH_CPU, ICON_COLOR, &cpu_txt, tint(cpu_ptr, 75.0, 95.0), &mut x, 16.0);

        let mem_txt = if d.mem_total > 0 {
            format!("MEM {} ({:.0}%)", d.mem_text(), d.mem_pct())
        } else {
            "MEM n/a".to_string()
        };
        seg(&painter, GLYPH_MEM, ICON_COLOR, &mem_txt, tint(d.mem_pct(), 85.0, 95.0), &mut x, 16.0);

        if d.disk_free > 0 && d.disk_pct > 0.0 {
            let disk_txt = format!("/ {} ({:.0}%)", d.disk_text(), d.disk_pct);
            seg(&painter, GLYPH_DISK, ICON_COLOR, &disk_txt, tint(d.disk_pct, 80.0, 90.0), &mut x, 16.0);
        }

        if !d.uptime.is_empty() {
            seg(&painter, GLYPH_UPTIME, ICON_COLOR, &d.uptime, BAR_TEXT, &mut x, 0.0);
        }
    }

    /// Session-window header strip with connection state.
    fn session_strip(&mut self, ui: &mut egui::Ui, strip: Rect) {
        let painter = ui.painter_at(strip);
        painter.rect_filled(strip, 0.0, theme::BG_PANEL());
        // Hairline under the strip.
        painter.line_segment(
            [
                egui::pos2(strip.left(), strip.bottom() - 0.5),
                egui::pos2(strip.right(), strip.bottom() - 0.5),
            ],
            Stroke::new(0.5_f32, theme::BORDER()),
        );

        let cy = strip.center().y;
        // Left: accent bar + session name.
        let bar = Rect::from_min_size(
            egui::pos2(strip.left() + 10.0, cy - 7.0),
            egui::vec2(3.0, 14.0),
        );
        painter.rect_filled(bar, 1.5, theme::ACCENT());
        painter.text(
            egui::pos2(bar.right() + 8.0, cy),
            Align2::LEFT_CENTER,
            &self.title,
            FontId::proportional(12.0),
            theme::TEXT(),
        );
        // Right side: connection label, quiet.
        painter.text(
            egui::pos2(strip.right() - 10.0, cy),
            Align2::RIGHT_CENTER,
            self.connection_label(),
            FontId::proportional(10.0),
            theme::TEXT_FAINT(),
        );
    }

    fn shell_body(&mut self, ui: &mut egui::Ui, rect: Rect) {
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, TERM_BG());

        let font_id = FontId::new(13.0, FontFamily::Monospace);
        let (cell_w, cell_h) = ui.fonts(|f| {
            (
                f.glyph_width(&font_id, 'M').max(1.0),
                f.row_height(&font_id).max(1.0),
            )
        });

        // Focus: click to focus, like a real terminal.
        let resp = ui.interact(rect, self.focus_id, egui::Sense::click());
        if resp.clicked() {
            resp.request_focus();
        }
        let focused = ui.memory(|m| m.has_focus(self.focus_id));

        let inner = rect.shrink(4.0);
        let cols = ((inner.width() / cell_w).floor() as i64).clamp(2, 500) as u16;
        let rows = ((inner.height() / cell_h).floor() as i64).clamp(2, 300) as u16;
        if cols != self.cols || rows != self.rows {
            self.cols = cols;
            self.rows = rows;
            self.parser.screen_mut().set_size(rows, cols);
            self.send(TermCmd::Resize { cols, rows });
        }

        // Wheel = scrollback.
        if resp.hovered() || focused {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy.abs() > 0.5 {
                let current = self.parser.screen().scrollback() as i32;
                let lines = (dy / (cell_h * 3.0)).round() as i32;
                let target = (current + lines).max(0) as usize;
                self.parser.screen_mut().set_scrollback(target);
            }
        }

        // Keyboard input when focused.
        if focused {
            let events = ui.input(|i| i.events.clone());
            let app_cursor = self.parser.screen().application_cursor();
            let bracketed = self.parser.screen().bracketed_paste();
            let mut bytes: Vec<u8> = Vec::new();
            for ev in &events {
                match ev {
                    egui::Event::Text(t) => bytes.extend_from_slice(t.as_bytes()),
                    egui::Event::Paste(text) => {
                        if bracketed {
                            bytes.extend_from_slice(b"\x1b[200~");
                            bytes.extend_from_slice(text.as_bytes());
                            bytes.extend_from_slice(b"\x1b[201~");
                        } else {
                            bytes.extend_from_slice(text.as_bytes());
                        }
                    }
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        // Ctrl+V is delivered as Event::Paste by the
                        // backend; don't also send 0x16.
                        if (modifiers.ctrl || modifiers.command)
                            && matches!(key, egui::Key::V)
                        {
                            continue;
                        }
                        if let Some(seq) = encode_key(key, *modifiers, app_cursor) {
                            bytes.extend_from_slice(&seq);
                        }
                    }
                    _ => {}
                }
            }
            if !bytes.is_empty() {
                // Typing always jumps back to the live screen.
                self.parser.screen_mut().set_scrollback(0);
                self.send(TermCmd::Write(bytes));
            }
        }

        self.paint(ui, inner, &font_id, cell_w, cell_h, focused);

        // Overlays for connection lifecycle.
        match &self.status {
            TermStatus::Connecting => {
                let fake = Rect::from_center_size(rect.center(), egui::vec2(1.0, 1.0));
                ui.put(fake, egui::Spinner::new().size(22.0));
                painter.text(
                    rect.center() + egui::vec2(0.0, 30.0),
                    egui::Align2::CENTER_CENTER,
                    format!("Connecting to {} …", self.connection_label()),
                    FontId::proportional(11.5),
                    Color32::from_gray(180),
                );
            }
            TermStatus::Closed(reason) => {
                painter.rect_filled(rect.shrink(1.0), 0.0, Color32::from_black_alpha(120));
                let card = Rect::from_center_size(rect.center(), egui::vec2(430.0, 150.0));
                painter.rect_filled(card, 12.0, theme::BG_CARD());
                painter.rect_stroke(
                    card,
                    12.0,
                    Stroke::new(1.0_f32, theme::BORDER_STRONG()),
                    StrokeKind::Inside,
                );
                painter.text(
                    card.center() + egui::vec2(0.0, -48.0),
                    egui::Align2::CENTER_CENTER,
                    "Disconnected",
                    FontId::proportional(14.0),
                    theme::TEXT(),
                );
                // Reason: real wrapped label so long OS errors stay inside
                // the card instead of spilling over its edges.
                let reason_rect = Rect::from_min_size(
                    card.min + egui::vec2(28.0, card.height() * 0.34),
                    egui::vec2(card.width() - 56.0, 48.0),
                );
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(reason_rect));
                child.vertical_centered(|ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(reason.clone())
                                .size(11.5)
                                .color(theme::TEXT_DIM()),
                        )
                        .wrap(),
                    );
                });
                let mut child = ui.new_child(
                    egui::UiBuilder::new().max_rect(
                        Rect::from_center_size(card.center() + egui::vec2(0.0, 44.0), egui::vec2(280.0, 30.0)),
                    ),
                );
                child.horizontal_centered(|ui| {
                    if ui.button("Reconnect").clicked() {
                        self.want_reconnect = true;
                    }
                    if ui.button("Close tab").clicked() {
                        self.want_close = true;
                    }
                });
            }
            TermStatus::Connected => {}
        }
    }

    fn paint(
        &mut self,
        ui: &egui::Ui,
        inner: Rect,
        font_id: &FontId,
        cell_w: f32,
        cell_h: f32,
        focused: bool,
    ) {
        let painter = ui.painter_at(inner);
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        let scrollback = screen.scrollback();
        let hide_cursor = screen.hide_cursor();

        let mono = font_id.clone();
        for row in 0..rows {
            let y = inner.top() + row as f32 * cell_h;
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = f32::INFINITY;
            // current run
            let mut run = String::new();
            let mut run_fg = TERM_FG();
            let mut run_underline = false;

            let flush = |job: &mut egui::text::LayoutJob,
                         run: &mut String,
                         fg: Color32,
                         underline: bool| {
                if run.is_empty() {
                    return;
                }
                let mut fmt = egui::TextFormat {
                    font_id: mono.clone(),
                    color: fg,
                    ..Default::default()
                };
                if underline {
                    fmt.underline = Stroke::new(1.0_f32, fg);
                }
                job.append(run, 0.0, fmt);
                run.clear();
            };

            for col in 0..cols {
                let x = inner.left() + col as f32 * cell_w;
                let cell = screen.cell(row, col);
                let (text, fg, bg, underline) = match cell {
                    Some(c) => {
                        if c.is_wide_continuation() {
                            continue;
                        }
                        let t = c.contents();
                        let t = if t.is_empty() { " " } else { t };
                        let mut fg = map_color(c.fgcolor(), TERM_FG(), c.bold());
                        let mut bg = map_color(c.bgcolor(), TERM_BG(), false);
                        if c.dim() {
                            fg = fg.gamma_multiply(0.72);
                        }
                        if c.inverse() {
                            std::mem::swap(&mut fg, &mut bg);
                        }
                        (t, fg, bg, c.underline())
                    }
                    None => (" ", TERM_FG(), TERM_BG(), false),
                };
                if bg != TERM_BG() {
                    painter.rect_filled(
                        Rect::from_min_size(egui::pos2(x, y), egui::vec2(cell_w, cell_h)),
                        0.0,
                        bg,
                    );
                }
                if fg != run_fg || underline != run_underline {
                    flush(&mut job, &mut run, run_fg, run_underline);
                    run_fg = fg;
                    run_underline = underline;
                }
                run.push_str(text);
            }
            flush(&mut job, &mut run, run_fg, run_underline);

            if !job.sections.is_empty() {
                let galley = ui.fonts(|f| f.layout_job(job));
                painter.galley(egui::pos2(inner.left(), y), galley, TERM_FG());
            }
        }

        // Cursor (hidden while scrolled back, like real terminals).
        if scrollback == 0 && !hide_cursor {
            let time = ui.input(|i| i.time);
            let blink_on = (time / 0.53) as i64 % 2 == 0;
            let crect = Rect::from_min_size(
                egui::pos2(
                    inner.left() + cursor_col as f32 * cell_w,
                    inner.top() + cursor_row as f32 * cell_h,
                ),
                egui::vec2(cell_w, cell_h),
            );
            if focused {
                if blink_on {
                    // Solid block cursor, like MobaXterm/xterm. A light wash
                    // keeps the glyph underneath readable.
                    painter.rect_filled(
                        crect,
                        0.0,
                        Color32::from_rgba_unmultiplied(229, 229, 229, 110),
                    );
                    painter.rect_stroke(
                        crect,
                        0.0,
                        Stroke::new(1.0_f32, Color32::from_gray(229)),
                        StrokeKind::Inside,
                    );
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(60));
            } else {
                painter.rect_stroke(
                    crect,
                    0.0,
                    Stroke::new(1.0_f32, Color32::from_gray(140)),
                    StrokeKind::Inside,
                );
            }
        }

        // Scrollback indicator.
        if scrollback > 0 {
            painter.text(
                egui::pos2(inner.right() - 8.0, inner.top() + 6.0),
                egui::Align2::RIGHT_TOP,
                format!("↑ {scrollback} lines — press any key to return"),
                FontId::proportional(10.0),
                Color32::from_gray(160),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Server monitor (MobaXterm-style strip under the shell)
// ---------------------------------------------------------------------------

// Tabler glyphs for the strip (codepoints resolved from the bundled font).
const GLYPH_USER: char = '\u{eb4d}'; // user
const GLYPH_CPU: char = '\u{ef8e}'; // cpu
const GLYPH_MEM: char = '\u{eb1f}'; // server (RAM)
const GLYPH_DISK: char = '\u{ea88}'; // database (disk)
const GLYPH_UPTIME: char = '\u{ebea}'; // history

/// One cheap POSIX probe, echoed with deterministic markers. Missing pieces
/// (macOS has no /proc) parse to "n/a" instead of failing the whole bar.
const MON_SCRIPT: &str = "echo U=$(whoami 2>/dev/null); \
echo H=$(hostname 2>/dev/null); \
echo C=$(nproc 2>/dev/null); \
echo L=$(cat /proc/loadavg 2>/dev/null); \
grep -E '^(MemTotal|MemAvailable):' /proc/meminfo 2>/dev/null; \
echo D=$(df -P / 2>/dev/null | tail -n 1); \
echo T=[$(uptime 2>/dev/null)]";

/// Parse the MON_SCRIPT output into a [`ServerData`].
fn parse_server_data(out: &str) -> Option<ServerData> {
    let mut d = ServerData::default();
    let mut saw_something = false;
    for raw in out.lines() {
        let line = raw.trim();
        saw_something = saw_something || !line.is_empty();
        if let Some(v) = line.strip_prefix("U=") {
            d.user = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("H=") {
            d.host = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("C=") {
            d.cores = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("L=") {
            d.load = v
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
        } else if let Some(v) = line.strip_prefix("MemTotal:") {
            d.mem_total = v.trim().split_whitespace().next().unwrap_or("0").parse().unwrap_or(0) * 1024;
        } else if let Some(v) = line.strip_prefix("MemAvailable:") {
            let avail = v.trim().split_whitespace().next().unwrap_or("0").parse::<u64>().unwrap_or(0) * 1024;
            d.mem_used = d.mem_total.saturating_sub(avail);
        } else if let Some(v) = line.strip_prefix("D=") {
            // df -P fields: fs blocks used avail capacity mount
            let f: Vec<&str> = v.split_whitespace().collect();
            if f.len() >= 5 {
                let total: u64 = f[1].parse().unwrap_or(0);
                let avail: u64 = f[3].parse().unwrap_or(0);
                d.disk_pct = f[4].trim_end_matches('%').parse().unwrap_or(0.0);
                d.disk_free = avail * 1024;
                d.disk_path = f.get(5).unwrap_or(&"/").to_string();
                let _ = total;
            }
        } else if let Some(v) = line.strip_prefix("T=[") {
            let body = v.strip_suffix(']').unwrap_or(v);
            // "… up 3 days,  2:15,  1 user, …" → keep the time segments
            // between "up" and the user/load-average tail.
            let tail = body.split(" up ").nth(1).unwrap_or("");
            let mut parts: Vec<String> = Vec::new();
            for seg in tail.split(", ") {
                let s = seg.trim();
                if s.is_empty() || s.contains(" user") || s.starts_with("load average") {
                    break;
                }
                parts.push(s.to_string());
            }
            d.uptime = parts.join(", ");
        }
    }
    saw_something.then_some(d)
}

/// Own low-rate SSH connection per tab: samples server health every 10 s.
/// Ends when the stop queue closes (tab replaced/closed) or the app exits.
async fn monitor_task(
    session: SshSession,
    logs: SharedLogs,
    ctx: egui::Context,
    evt_tx: UnboundedSender<TermEvent>,
    mut cmd_rx: UnboundedReceiver<MonCmd>,
) {
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
    let mut handle: Option<ssh::SshHandle> = None;

    loop {
        // One sample per tick; stop whenever asked.
        let mut stop = false;
        tokio::select! {
            _ = tokio::time::sleep(INTERVAL) => {}
            r = cmd_rx.recv() => {
                stop = r.is_none() || matches!(r, Some(MonCmd::Stop));
            }
        }
        if stop {
            break;
        }

        if handle.is_none() {
            let (ftx, _frx) = tokio::sync::mpsc::unbounded_channel();
            match ssh::connect(&session, logs.clone(), ftx).await {
                Ok(h) => handle = Some(h),
                Err(_) => {
                    // Server unreachable: report empty; retry next tick.
                    let _ = evt_tx.send(TermEvent::Mon(None));
                    ctx.request_repaint();
                    continue;
                }
            }
        }

        let data = if let Some(h) = handle.as_ref() {
            sample_server(h).await
        } else {
            None
        };
        if data.is_none() {
            // Connection went stale — reconnect next cycle.
            handle = None;
        }
        let _ = evt_tx.send(TermEvent::Mon(data));
        ctx.request_repaint();
    }
}

/// Run MON_SCRIPT in a throwaway exec channel and parse the output.
async fn sample_server(handle: &ssh::SshHandle) -> Option<ServerData> {
    let mut ch = handle.channel_open_session().await.ok()?;
    ch.exec(false, MON_SCRIPT).await.ok()?;
    let mut out = Vec::new();
    loop {
        match ch.wait().await {
            Some(ChannelMsg::Data { data }) => out.extend_from_slice(&data),
            Some(ChannelMsg::ExtendedData { data, .. }) => out.extend_from_slice(&data),
            Some(ChannelMsg::ExitStatus { .. }) => {}
            Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
            _ => {}
        }
    }
    let _ = ch.close().await;
    parse_server_data(&String::from_utf8_lossy(&out))
}

// ---------------------------------------------------------------------------
// Async shell task
// ---------------------------------------------------------------------------

async fn shell_task(
    session: SshSession,
    logs: SharedLogs,
    ctx: egui::Context,
    evt_tx: UnboundedSender<TermEvent>,
    mut cmd_rx: UnboundedReceiver<TermCmd>,
    cols: u16,
    rows: u16,
) {
    let closed = |evt_tx: &UnboundedSender<TermEvent>, ctx: &egui::Context, msg: String| {
        let _ = evt_tx.send(TermEvent::Closed(msg));
        ctx.request_repaint();
    };

    // forwarded-tcpip queue is unused for shells.
    let (ftx, _frx) = tokio::sync::mpsc::unbounded_channel();
    let handle = match ssh::connect(&session, logs.clone(), ftx).await {
        Ok(h) => h,
        Err(e) => return closed(&evt_tx, &ctx, format!("{e:#}")),
    };

    let mut ch = match handle.channel_open_session().await {
        Ok(c) => c,
        Err(e) => return closed(&evt_tx, &ctx, format!("Cannot open session channel: {e}")),
    };
    if let Err(e) = ch
        .request_pty(true, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
        .await
    {
        return closed(&evt_tx, &ctx, format!("PTY request failed: {e}"));
    }
    if let Err(e) = ch.request_shell(true).await {
        return closed(&evt_tx, &ctx, format!("Shell request failed: {e}"));
    }

    let _ = evt_tx.send(TermEvent::Connected);
    ctx.request_repaint();

    // Loop layout keeps all `ch` usage OUT of select! handlers so future
    // borrows can never conflict.
    let mut pending: Vec<Vec<u8>> = Vec::new();
    let mut resize: Option<(u16, u16)> = None;
    let mut exit_status: Option<u32> = None;
    let mut quit = false;
    while !quit {
        if let Some((c, r)) = resize.take() {
            if let Err(e) = ch.window_change(c as u32, r as u32, 0, 0).await {
                closed(&evt_tx, &ctx, format!("Resize failed: {e}"));
                return;
            }
        }
        for b in pending.drain(..) {
            if let Err(e) = ch.data_bytes(b).await {
                closed(&evt_tx, &ctx, format!("Write failed: {e}"));
                return;
            }
        }
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(TermCmd::Write(b)) => pending.push(b),
                Some(TermCmd::Resize { cols, rows }) => resize = Some((cols, rows)),
                Some(TermCmd::Close) | None => quit = true,
            },
            msg = ch.wait() => match msg {
                Some(ChannelMsg::Data { data }) => {
                    let _ = evt_tx.send(TermEvent::Output(data.to_vec()));
                    ctx.request_repaint();
                }
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    let _ = evt_tx.send(TermEvent::Output(data.to_vec()));
                    ctx.request_repaint();
                }
                Some(ChannelMsg::ExitStatus { exit_status: s }) => exit_status = Some(s),
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => quit = true,
                _ => {}
            }
        }
    }

    let _ = ch.eof().await;
    let _ = ch.close().await;
    let reason = match exit_status {
        Some(0) => "Connection closed (exit code 0)".to_string(),
        Some(s) => format!("Connection closed (exit code {s})"),
        None => "Connection closed".to_string(),
    };
    closed(&evt_tx, &ctx, reason);
}

// ---------------------------------------------------------------------------
// Key encoding (xterm sequences)
// ---------------------------------------------------------------------------

/// Encodes control keys/special keys to xterm byte sequences. Printable text
/// arrives through `egui::Event::Text` and is not handled here.
pub fn encode_key(key: &egui::Key, mods: egui::Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    use egui::Key as K;
    let ctrl = mods.ctrl || mods.command;
    let alt = mods.alt && !ctrl;

    // Ctrl+letter / ctrl+punctuation → ASCII control codes. Non-character
    // keys (arrows, F-keys, …) fall through so their ctrl form is encoded
    // as an xterm modifier parameter below.
    if ctrl {
        if *key == K::Space {
            return Some(vec![0x00]);
        }
        if let Some(c) = key_char(key) {
            let b = match c {
                '@' | '2' => 0x00,
                '3'..='7' => c as u8 - 24,
                '8' | '?' => 0x7f,
                '[' | '\\' | ']' | '^' | '_' | '/' => (c as u8) & 0x1f,
                c if c.is_ascii_alphabetic() => (c as u8 | 0x20) & 0x1f,
                _ => return None,
            };
            return Some(vec![b]);
        }
    }

    // Alt+char → ESC prefix (meta key).
    if alt {
        if let Some(c) = key_char(key) {
            let mut v = vec![0x1b];
            let mut buf = [0u8; 4];
            v.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            return Some(v);
        }
    }

    // xterm modifier parameter for cursor keys:
    // 1 + shift(1) + alt(2) + ctrl(4).
    let m = 1 + i32::from(mods.shift) + 2 * i32::from(mods.alt) + 4 * i32::from(mods.ctrl);
    let cursor_seq = |letter: char, m: i32, app_cursor: bool| -> Vec<u8> {
        if m == 1 {
            if app_cursor {
                format!("\x1bO{letter}").into_bytes()
            } else {
                format!("\x1b[{letter}").into_bytes()
            }
        } else {
            format!("\x1b[1;{m}{letter}").into_bytes()
        }
    };
    let tilde_seq = |num: u8, m: i32| -> Vec<u8> {
        if m == 1 {
            format!("\x1b[{num}~").into_bytes()
        } else {
            format!("\x1b[{num};{m}~").into_bytes()
        }
    };

    let seq: Vec<u8> = match key {
        K::Enter => b"\r".to_vec(),
        K::Escape => b"\x1b".to_vec(),
        K::Backspace => b"\x7f".to_vec(),
        K::Tab => {
            if mods.shift {
                b"\x1b[Z".to_vec()
            } else {
                b"\t".to_vec()
            }
        }
        K::ArrowUp => cursor_seq('A', m, app_cursor),
        K::ArrowDown => cursor_seq('B', m, app_cursor),
        K::ArrowRight => cursor_seq('C', m, app_cursor),
        K::ArrowLeft => cursor_seq('D', m, app_cursor),
        K::Home => {
            if m == 1 && app_cursor {
                b"\x1bOH".to_vec()
            } else if m == 1 {
                b"\x1b[H".to_vec()
            } else {
                format!("\x1b[1;{m}H").into_bytes()
            }
        }
        K::End => {
            if m == 1 && app_cursor {
                b"\x1bOF".to_vec()
            } else if m == 1 {
                b"\x1b[F".to_vec()
            } else {
                format!("\x1b[1;{m}F").into_bytes()
            }
        }
        K::Insert => tilde_seq(2, m),
        K::Delete => tilde_seq(3, m),
        K::PageUp => tilde_seq(5, m),
        K::PageDown => tilde_seq(6, m),
        K::F1 => b"\x1bOP".to_vec(),
        K::F2 => b"\x1bOQ".to_vec(),
        K::F3 => b"\x1bOR".to_vec(),
        K::F4 => b"\x1bOS".to_vec(),
        K::F5 => tilde_seq(15, m),
        K::F6 => tilde_seq(17, m),
        K::F7 => tilde_seq(18, m),
        K::F8 => tilde_seq(19, m),
        K::F9 => tilde_seq(20, m),
        K::F10 => tilde_seq(21, m),
        K::F11 => tilde_seq(23, m),
        K::F12 => tilde_seq(24, m),
        _ => return None,
    };
    Some(seq)
}

fn key_char(key: &egui::Key) -> Option<char> {
    use egui::Key as K;
    let c = match key {
        K::A => 'a', K::B => 'b', K::C => 'c', K::D => 'd', K::E => 'e',
        K::F => 'f', K::G => 'g', K::H => 'h', K::I => 'i', K::J => 'j',
        K::K => 'k', K::L => 'l', K::M => 'm', K::N => 'n', K::O => 'o',
        K::P => 'p', K::Q => 'q', K::R => 'r', K::S => 's', K::T => 't',
        K::U => 'u', K::V => 'v', K::W => 'w', K::X => 'x', K::Y => 'y',
        K::Z => 'z',
        K::Num0 => '0', K::Num1 => '1', K::Num2 => '2', K::Num3 => '3',
        K::Num4 => '4', K::Num5 => '5', K::Num6 => '6', K::Num7 => '7',
        K::Num8 => '8', K::Num9 => '9',
        K::Space => ' ',
        K::Minus => '-',
        K::Plus => '+',
        K::Equals => '=',
        K::OpenBracket => '[',
        K::CloseBracket => ']',
        K::Backslash => '\\',
        K::Semicolon => ';',
        K::Quote => '\'',
        K::Comma => ',',
        K::Period => '.',
        K::Slash => '/',
        K::Backtick => '`',
        _ => return None,
    };
    Some(c)
}

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

/// Maps a vt100 color to the on-screen color. Indexed colors use a modern
/// dark palette; bold promotes the 8 base colors to their bright variants.
fn map_color(c: vt100::Color, default: Color32, bold: bool) -> Color32 {
    match c {
        vt100::Color::Default => {
            if bold && default == TERM_FG() {
                Color32::WHITE
            } else {
                default
            }
        }
        vt100::Color::Idx(i) => idx_color(i, bold),
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

fn idx_color(i: u8, bold: bool) -> Color32 {
    // The classic xterm 16-colour palette MobaXterm ships as its default
    // ("MobaXterm") colour scheme: pure black canvas, saturated ANSI hues and
    // an #E5E5E5 foreground.
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),       // 0 black — #000000
        (205, 0, 0),     // 1 red — #CD0000
        (0, 205, 0),     // 2 green — #00CD00
        (205, 205, 0),   // 3 yellow — #CDCD00
        (0, 0, 238),     // 4 blue — #0000EE
        (205, 0, 205),   // 5 magenta — #CD00CD
        (0, 205, 205),   // 6 cyan — #00CDCD
        (229, 229, 229), // 7 white — #E5E5E5
        (127, 127, 127), // 8 bright black — #7F7F7F
        (255, 0, 0),     // 9 bright red — #FF0000
        (0, 255, 0),     // 10 bright green — #00FF00
        (255, 255, 0),   // 11 bright yellow — #FFFF00
        (92, 92, 255),   // 12 bright blue — #5C5CFF
        (255, 0, 255),   // 13 bright magenta — #FF00FF
        (0, 255, 255),   // 14 bright cyan — #00FFFF
        (255, 255, 255), // 15 bright white — #FFFFFF
    ];
    let i = if bold && i < 8 { i + 8 } else { i };
    if i < 16 {
        let (r, g, b) = BASE[i as usize];
        return Color32::from_rgb(r, g, b);
    }
    if i < 232 {
        let i = i - 16;
        let r = i / 36;
        let g = (i % 36) / 6;
        let b = i % 6;
        let lv = |v: u8| -> u8 {
            if v == 0 {
                0
            } else {
                55 + v * 40
            }
        };
        return Color32::from_rgb(lv(r), lv(g), lv(b));
    }
    let v = 8 + (i - 232) * 10;
    Color32::from_rgb(v, v, v)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Key, Modifiers};

    fn no_mods() -> Modifiers {
        Modifiers::default()
    }

    #[test]
    fn ctrl_letters_map_to_control_codes() {
        assert_eq!(
            encode_key(&Key::C, Modifiers { ctrl: true, ..no_mods() }, false),
            Some(vec![0x03])
        );
        assert_eq!(
            encode_key(&Key::D, Modifiers { ctrl: true, ..no_mods() }, false),
            Some(vec![0x04])
        );
        assert_eq!(
            encode_key(&Key::OpenBracket, Modifiers { ctrl: true, ..no_mods() }, false),
            Some(vec![0x1b])
        );
    }

    #[test]
    fn special_keys() {
        assert_eq!(encode_key(&Key::Enter, no_mods(), false), Some(b"\r".to_vec()));
        assert_eq!(encode_key(&Key::Backspace, no_mods(), false), Some(vec![0x7f]));
        assert_eq!(encode_key(&Key::Escape, no_mods(), false), Some(vec![0x1b]));
        assert_eq!(encode_key(&Key::Tab, no_mods(), false), Some(b"\t".to_vec()));
    }

    #[test]
    fn arrows_respect_application_cursor() {
        assert_eq!(encode_key(&Key::ArrowUp, no_mods(), false), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(&Key::ArrowUp, no_mods(), true), Some(b"\x1bOA".to_vec()));
        assert_eq!(
            encode_key(&Key::ArrowUp, Modifiers { ctrl: true, ..no_mods() }, false),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    #[test]
    fn alt_chars_get_escape_prefix() {
        assert_eq!(
            encode_key(&Key::B, Modifiers { alt: true, ..no_mods() }, false),
            Some(b"\x1bb".to_vec())
        );
    }

    #[test]
    fn printable_keys_are_not_encoded_here() {
        // Printable text comes from Event::Text; encoding it here would
        // double the input.
        assert_eq!(encode_key(&Key::A, no_mods(), false), None);
    }

    #[test]
    fn ansi_palette_bounds() {
        assert_eq!(idx_color(1, false), Color32::from_rgb(205, 0, 0));
        assert_eq!(idx_color(9, false), Color32::from_rgb(255, 0, 0));
        assert_eq!(idx_color(16, false), Color32::from_rgb(0, 0, 0));
        assert_eq!(idx_color(231, false), Color32::from_rgb(255, 255, 255));
        assert_eq!(idx_color(255, false), Color32::from_rgb(238, 238, 238));
    }
}
