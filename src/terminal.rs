//! Interactive SSH shell in a tab: a real PTY session on the server,
//! rendered locally through a vt100 terminal emulator (colors, cursor,
//! scrollback, alternate screen), drawn with egui.

use crate::log::SharedLogs;
use crate::session::SshSession;
use crate::ssh;
use crate::theme;
use eframe::egui::{self, Color32, FontFamily, FontId, Rect, Stroke, StrokeKind};
use russh::ChannelMsg;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

const SCROLLBACK_LINES: usize = 4000;
const TERM_BG: Color32 = Color32::from_rgb(9, 13, 24);
const TERM_FG: Color32 = Color32::from_rgb(214, 222, 240);

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
        let (cols, rows) = (self.cols, self.rows);
        self.rt.spawn(async move {
            shell_task(session, logs, ctx, evt_tx, cmd_rx, cols, rows).await;
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
    }

    /// Drain async events into the parser. Returns true when repainting is
    /// needed for a reason other than cursor blink.
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
                TermEvent::Closed(reason) => {
                    self.status = TermStatus::Closed(reason);
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
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, TERM_BG);

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

        let inner = rect.shrink(6.0);
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
                    FontId::proportional(13.0),
                    theme::TEXT_DIM,
                );
            }
            TermStatus::Closed(reason) => {
                painter.rect_filled(rect.shrink(1.0), 0.0, Color32::from_black_alpha(120));
                let card = Rect::from_center_size(rect.center(), egui::vec2(430.0, 132.0));
                painter.rect_filled(card, 12.0, theme::BG_CARD);
                painter.rect_stroke(
                    card,
                    12.0,
                    Stroke::new(1.0_f32, theme::BORDER_STRONG),
                    StrokeKind::Inside,
                );
                painter.text(
                    card.center() + egui::vec2(0.0, -34.0),
                    egui::Align2::CENTER_CENTER,
                    "Disconnected",
                    FontId::proportional(16.0),
                    theme::TEXT,
                );
                painter.text(
                    card.center() + egui::vec2(0.0, -10.0),
                    egui::Align2::CENTER_CENTER,
                    reason.clone(),
                    FontId::proportional(12.5),
                    theme::TEXT_DIM,
                );
                let mut child = ui.new_child(
                    egui::UiBuilder::new().max_rect(
                        Rect::from_center_size(card.center() + egui::vec2(0.0, 34.0), egui::vec2(280.0, 30.0)),
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
            let mut run_fg = TERM_FG;
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
                        let mut fg = map_color(c.fgcolor(), TERM_FG, c.bold());
                        let mut bg = map_color(c.bgcolor(), TERM_BG, false);
                        if c.dim() {
                            fg = fg.gamma_multiply(0.72);
                        }
                        if c.inverse() {
                            std::mem::swap(&mut fg, &mut bg);
                        }
                        (t, fg, bg, c.underline())
                    }
                    None => (" ", TERM_FG, TERM_BG, false),
                };
                if bg != TERM_BG {
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
                painter.galley(egui::pos2(inner.left(), y), galley, TERM_FG);
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
                    painter.rect_filled(crect, 1.0, Color32::from_rgba_unmultiplied(230, 236, 255, 110));
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(60));
            } else {
                painter.rect_stroke(
                    crect,
                    1.0,
                    Stroke::new(1.0_f32, theme::TEXT_FAINT),
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
                FontId::proportional(11.0),
                theme::TEXT_FAINT,
            );
        }
    }
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
            if bold && default == TERM_FG {
                Color32::from_rgb(240, 245, 255)
            } else {
                default
            }
        }
        vt100::Color::Idx(i) => idx_color(i, bold),
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

fn idx_color(i: u8, bold: bool) -> Color32 {
    // 16 base colors tuned for a dark UI (One Dark family).
    const BASE: [(u8, u8, u8); 16] = [
        (36, 42, 58),    // 0 black
        (224, 108, 117), // 1 red
        (152, 195, 121), // 2 green
        (229, 192, 123), // 3 yellow
        (97, 175, 239),  // 4 blue
        (198, 120, 221), // 5 magenta
        (86, 182, 194),  // 6 cyan
        (220, 223, 228), // 7 white
        (92, 99, 112),   // 8 bright black
        (255, 123, 134), // 9 bright red
        (179, 224, 140), // 10 bright green
        (245, 209, 147), // 11 bright yellow
        (125, 195, 255), // 12 bright blue
        (216, 160, 239), // 13 bright magenta
        (102, 217, 227), // 14 bright cyan
        (255, 255, 255), // 15 bright white
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
        assert_eq!(idx_color(1, false), Color32::from_rgb(224, 108, 117));
        assert_eq!(idx_color(9, false), Color32::from_rgb(255, 123, 134));
        assert_eq!(idx_color(16, false), Color32::from_rgb(0, 0, 0));
        assert_eq!(idx_color(231, false), Color32::from_rgb(255, 255, 255));
        assert_eq!(idx_color(255, false), Color32::from_rgb(238, 238, 238));
    }
}
