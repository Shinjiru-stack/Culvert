//! Design system for the app: color palette, typography, global egui style
//! and a procedurally generated application icon.

// Palette getters keep the old const names (ACCENT(), TEXT(), …) so the
// ~180 call sites read as before; that's deliberately non-snake-case.
#![allow(non_snake_case)]

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow,
    Stroke, TextStyle, Visuals,
};
use eframe::egui::style::WidgetVisuals;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Icon glyphs
// ---------------------------------------------------------------------------
/// Tabler Icons glyphs (MIT, bundled webfont in `assets/fonts/tabler-icons.ttf`)
/// — shared line icons for the app shell (toolbar, rail, terminal quick-bar)
/// and the docked SFTP browser.
pub mod tglyph {
    /// terminal-2 (session / shell)
    pub const SESSION: char = '\u{ebef}';
    /// arrows-exchange (tunnel)
    pub const TUNNEL: char = '\u{f1f4}';
    /// columns-2 (split view)
    pub const SPLIT: char = '\u{f6d5}';
    /// activity (pulse)
    pub const ACTIVITY: char = '\u{ed23}';
    /// sun / moon (theme switch — pick the theme you'll get)
    pub const SUN: char = '\u{eb30}';
    pub const MOON: char = '\u{eaf8}';
    /// server-2 / folder (collapsed rail tiles)
    pub const RAIL_SESSIONS: char = '\u{f07c}';
    pub const RAIL_FILES: char = '\u{eaad}';
    /// chevron-right / left (rail expand, sidebar collapse)
    pub const CHEVRON_RIGHT: char = '\u{ea61}';
    pub const CHEVRON_LEFT: char = '\u{ea60}';
    pub const CHEVRON_UP: char = '\u{ea62}';
    /// plug (quick connect)
    pub const PLUG: char = '\u{ebd9}';

    // --- File-browser dock + terminal quick-bar tools ---
    /// upload (multi-file upload)
    pub const UPLOAD: char = '\u{eb47}';
    /// download (multi-file download)
    pub const DOWNLOAD: char = '\u{ea96}';
    /// trash (delete selection)
    pub const TRASH: char = '\u{eb41}';
    /// copy (copy selection path)
    pub const COPY: char = '\u{ea7a}';
    /// clipboard-text (paste / insert)
    pub const PASTE: char = '\u{f089}';
    /// refresh (relist current directory)
    pub const REFRESH: char = '\u{eb13}';
    /// eye / eye-off (show-hidden-files toggle)
    pub const EYE: char = '\u{ea9a}';
    pub const EYE_OFF: char = '\u{ecf0}';
    /// file-plus (create new file)
    pub const FILE_PLUS: char = '\u{eaa0}';
    /// folder-plus (create new folder)
    pub const FOLDER_PLUS: char = '\u{eaab}';
    /// pencil (local editors)
    pub const PENCIL: char = '\u{eb04}';
    /// eraser (clear terminal screen)
    pub const ERASER: char = '\u{eb8b}';
    /// reload (reconnect session)
    pub const RELOAD: char = '\u{f3ae}';

    /// Directory / file row glyphs for the SFTP listing.
    pub const FOLDER_ROW: char = '\u{eaad}';
    pub const FILE_ROW: char = '\u{eaa4}';

    /// AI Copilot: sparkles on the toolbar tile, robot + bolt in the panel,
    /// shield on approval/security cards, gear/history/send in the chrome.
    pub const COPILOT: char = '\u{f6d7}';
    pub const BOT: char = '\u{f00b}';
    pub const BOLT: char = '\u{ea38}';
    pub const SHIELD: char = '\u{eb24}';
    pub const SEND: char = '\u{eb1e}';
    pub const GEAR: char = '\u{eb20}';
    pub const HISTORY: char = '\u{ebea}';
    /// x (close)
    pub const CLOSE: char = '\u{78}';
}

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------
// Light: modern macOS-style white chrome, Linear-like accents; Dark: GitHub
// Dark family surfaces with the same indigo accent family, tuned so text,
// chips and status colors stay readable on near-black.

use std::sync::atomic::{AtomicBool, Ordering};

pub struct Palette {
    pub bg_window: Color32,
    pub bg_panel: Color32,
    pub bg_panel_alt: Color32,
    pub bg_card: Color32,
    pub bg_card_sel: Color32,
    pub bg_input: Color32,
    pub bg_btn: Color32,
    pub bg_btn_hover: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub accent: Color32,
    pub accent_hover: Color32,
    pub accent_soft: Color32,
    pub accent_soft_hover: Color32,
    pub accent_text: Color32,
    pub text: Color32,
    pub text_dim: Color32,
    pub text_faint: Color32,
    pub success: Color32,
    pub success_bg: Color32,
    pub warning: Color32,
    pub warning_bg: Color32,
    pub danger: Color32,
    pub danger_btn: Color32,
    pub danger_btn_hover: Color32,
    pub danger_bg: Color32,
    pub info: Color32,
    pub info_bg: Color32,
    pub violet: Color32,
    pub violet_bg: Color32,
    pub sb_bg: Color32,
    pub sb_text: Color32,
    pub sb_text_faint: Color32,
}

#[rustfmt::skip]
pub const LIGHT: Palette = Palette {
    bg_window:          Color32::from_rgb(255, 255, 255), // #FFFFFF content
    bg_panel:           Color32::from_rgb(240, 240, 240), // #F0F0F0 chrome
    bg_panel_alt:       Color32::from_rgb(233, 233, 233), // #E9E9E9
    bg_card:            Color32::from_rgb(255, 255, 255),
    bg_card_sel:        Color32::from_rgb(204, 232, 255), // #CCE8FF
    bg_input:           Color32::from_rgb(255, 255, 255),
    bg_btn:             Color32::from_rgb(240, 240, 240),
    bg_btn_hover:       Color32::from_rgb(229, 241, 251), // #E5F1FB
    border:             Color32::from_rgb(217, 217, 217), // #D9D9D9
    border_strong:      Color32::from_rgb(173, 173, 173), // #ADADAD
    accent:             Color32::from_rgb(0, 120, 215),   // #0078D7
    accent_hover:       Color32::from_rgb(0, 99, 177),
    accent_soft:        Color32::from_rgb(229, 241, 251),
    accent_soft_hover:  Color32::from_rgb(204, 232, 255),
    accent_text:        Color32::from_rgb(0, 90, 158),
    text:               Color32::from_rgb(26, 26, 26),    // #1A1A1A
    text_dim:           Color32::from_rgb(90, 90, 90),
    text_faint:         Color32::from_rgb(138, 138, 138),
    success:            Color32::from_rgb(16, 124, 16),   // #107C10
    success_bg:         Color32::from_rgb(223, 246, 221),
    warning:            Color32::from_rgb(157, 93, 0),    // #9D5D00
    warning_bg:         Color32::from_rgb(255, 244, 206),
    danger:             Color32::from_rgb(196, 43, 28),   // #C42B1C
    danger_btn:         Color32::from_rgb(196, 43, 28),
    danger_btn_hover:   Color32::from_rgb(164, 38, 25),
    danger_bg:          Color32::from_rgb(253, 231, 233),
    info:               Color32::from_rgb(0, 120, 215),   // #0078D7
    info_bg:            Color32::from_rgb(229, 241, 251),
    violet:             Color32::from_rgb(136, 23, 152),  // #881798
    violet_bg:          Color32::from_rgb(245, 233, 247),
    sb_bg:              Color32::from_rgb(240, 240, 240), // #F0F0F0
    sb_text:            Color32::from_rgb(60, 60, 60),
    sb_text_faint:      Color32::from_rgb(118, 118, 118),
};

#[rustfmt::skip]
pub const DARK: Palette = Palette {
    bg_window:          Color32::from_rgb(13, 17, 23),    // #0D1117
    bg_panel:           Color32::from_rgb(22, 27, 34),    // #161B22
    bg_panel_alt:       Color32::from_rgb(28, 33, 40),    // #1C2128
    bg_card:            Color32::from_rgb(28, 33, 40),    // #1C2128
    bg_card_sel:        Color32::from_rgb(38, 48, 74),
    bg_input:           Color32::from_rgb(13, 17, 23),    // #0D1117
    bg_btn:             Color32::from_rgb(33, 38, 45),    // #21262D
    bg_btn_hover:       Color32::from_rgb(44, 51, 60),
    border:             Color32::from_rgb(48, 54, 61),    // #30363D
    border_strong:      Color32::from_rgb(72, 80, 90),
    accent:             Color32::from_rgb(88, 112, 250),  // brighter indigo for dark
    accent_hover:       Color32::from_rgb(108, 132, 255),
    accent_soft:        Color32::from_rgb(38, 48, 74),
    accent_soft_hover:  Color32::from_rgb(48, 60, 92),
    accent_text:        Color32::from_rgb(157, 176, 255),
    text:               Color32::from_rgb(230, 237, 243), // #E6EDF3
    text_dim:           Color32::from_rgb(155, 166, 180),
    text_faint:         Color32::from_rgb(110, 119, 131),
    success:            Color32::from_rgb(63, 185, 80),   // #3FB950
    success_bg:         Color32::from_rgb(19, 47, 28),
    warning:            Color32::from_rgb(210, 153, 34),  // #D29922
    warning_bg:         Color32::from_rgb(64, 49, 9),
    danger:             Color32::from_rgb(248, 81, 73),   // #F85149
    danger_btn:         Color32::from_rgb(218, 54, 51),
    danger_btn_hover:   Color32::from_rgb(248, 81, 73),
    danger_bg:          Color32::from_rgb(61, 22, 24),
    info:               Color32::from_rgb(88, 166, 255),  // #58A6FF
    info_bg:            Color32::from_rgb(17, 36, 60),
    violet:             Color32::from_rgb(171, 125, 248), // #AB7DF8
    violet_bg:          Color32::from_rgb(48, 33, 77),
    sb_bg:              Color32::from_rgb(1, 4, 9),       // #010409
    sb_text:            Color32::from_rgb(139, 148, 158),
    sb_text_faint:      Color32::from_rgb(88, 96, 105),
};

static DARK_MODE: AtomicBool = AtomicBool::new(false);

/// True while the dark palette is active.
pub fn is_dark() -> bool {
    DARK_MODE.load(Ordering::Relaxed)
}

/// Palette-resolved color getters — same names as the old consts, so call
/// sites read `theme::ACCENT()` where they previously read `theme::ACCENT`.
macro_rules! tint {
    ($($name:ident => $field:ident),* $(,)?) => {
        $(pub fn $name() -> Color32 {
            if is_dark() { DARK.$field } else { LIGHT.$field }
        })*
    };
}

tint! {
    BG_WINDOW => bg_window,
    BG_PANEL => bg_panel,
    BG_PANEL_ALT => bg_panel_alt,
    BG_CARD => bg_card,
    BG_CARD_SEL => bg_card_sel,
    BG_INPUT => bg_input,
    BG_BTN => bg_btn,
    BG_BTN_HOVER => bg_btn_hover,
    BORDER => border,
    BORDER_STRONG => border_strong,
    ACCENT => accent,
    ACCENT_HOVER => accent_hover,
    ACCENT_SOFT => accent_soft,
    ACCENT_SOFT_HOVER => accent_soft_hover,
    ACCENT_TEXT => accent_text,
    TEXT => text,
    TEXT_DIM => text_dim,
    TEXT_FAINT => text_faint,
    SUCCESS => success,
    SUCCESS_BG => success_bg,
    WARNING => warning,
    WARNING_BG => warning_bg,
    DANGER => danger,
    DANGER_BTN => danger_btn,
    DANGER_BTN_HOVER => danger_btn_hover,
    DANGER_BG => danger_bg,
    INFO => info,
    INFO_BG => info_bg,
    VIOLET => violet,
    VIOLET_BG => violet_bg,
    SB_BG => sb_bg,
    SB_TEXT => sb_text,
    SB_TEXT_FAINT => sb_text_faint,
}

fn pref_file() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("culvert").join("theme.txt"))
}

/// Load the persisted preference ("dark" / "light"); applied at startup.
pub fn init() {
    if let Some(path) = pref_file() {
        let dark = std::fs::read_to_string(path)
            .map(|s| s.trim().eq_ignore_ascii_case("dark"))
            .unwrap_or(false);
        DARK_MODE.store(dark, Ordering::Relaxed);
    }
}

/// Switch palette and persist the choice.
pub fn set_dark(dark: bool) {
    DARK_MODE.store(dark, Ordering::Relaxed);
    if let Some(path) = pref_file() {
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(std::path::Path::new(".")));
        let _ = std::fs::write(&path, if dark { "dark" } else { "light" });
    }
}

/// Glyph font id for Tabler icon codepoints (`\u{ebef}` terminals etc.).
pub fn icon(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("tabler-icons".into()))
}

/// Named text style for the big page title.
pub fn h1() -> TextStyle {
    TextStyle::Name("app.h1".into())
}

/// Named text style for section headings.
pub fn h2() -> TextStyle {
    TextStyle::Name("app.h2".into())
}

/// Apply the whole design system to a context. Call once at startup.
pub fn apply(ctx: &egui::Context) {
    init();
    install_fonts(ctx);

    // Follow the chosen palette explicitly so an OS theme swap cannot drop
    // our named styles mid-session.
    ctx.set_theme(if is_dark() {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });

    // Keep the OS window dressing (win11 frame/shadow, dark-mode title bar)
    // in step with our palette. Without this a dark app keeps the light
    // system title bar — and the window border/shadow stays light too.
    sync_os_window_theme(ctx);

    let mut style = (*ctx.style()).clone();
    style.visuals = visuals();
    style.text_styles = text_styles();
    {
        let s = &mut style.spacing;
        s.item_spacing = egui::vec2(6.0, 3.0);
        s.button_padding = egui::vec2(7.0, 2.0);
        s.window_margin = Margin::same(8);
        s.menu_margin = Margin::same(3);
        s.indent = 12.0;
        s.interact_size = egui::vec2(28.0, 20.0);
        s.combo_width = 120.0;
        s.tooltip_width = 420.0;
    }
    ctx.set_style(style);
}

/// Ask the OS window manager to match our palette (dark/light title bar and
/// frame). egui maps this to winit's `set_theme`, which on Windows toggles
/// `DWMWA_USE_IMMERSIVE_DARK_MODE`. Safe to call on every theme change.
pub fn sync_os_window_theme(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(if is_dark() {
        egui::SystemTheme::Dark
    } else {
        egui::SystemTheme::Light
    }));
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "Inter".to_owned(),
        Arc::new(FontData::from_static(include_bytes!("../assets/fonts/Inter.ttf"))),
    );
    fonts.font_data.insert(
        "JetBrainsMono".to_owned(),
        Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/JetBrainsMono-Regular.ttf"
        ))),
    );
    // Tabler Icons webfont (MIT): professional line icons drawn as glyphs.
    // Lives in its own family (never in the text fallback chain so its PUA
    // codepoints stay out of normal text shaping).
    fonts.font_data.insert(
        "TablerIcons".to_owned(),
        Arc::new(FontData::from_static(include_bytes!(
            "../assets/fonts/tabler-icons.ttf"
        ))),
    );
    fonts
        .families
        .entry(FontFamily::Name("tabler-icons".into()))
        .or_default()
        .insert(0, "TablerIcons".to_owned());
    // Prepend our fonts, keeping the bundled fallbacks (emoji etc.) behind them.
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "Inter".to_owned());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .insert(0, "JetBrainsMono".to_owned());
    // System fonts as last-resort fallbacks: Segoe UI Symbol covers glyphs
    // missing from the bundled fonts (●, ×, triangles, arrows…) so they
    // render instead of tofu boxes on Windows.
    // MobaXterm's own type stack: Segoe UI for the chrome, Consolas for the
    // shell. These are Windows system fonts referenced in place — nothing is
    // redistributed. Segoe UI Symbol stays as a glyph fallback so symbols
    // (●, ×, triangles, arrows...) render instead of tofu boxes.
    for (name, path) in [
        ("SegoeUISymbol", "C:\\Windows\\Fonts\\seguisym.ttf"),
        ("SegoeUI", "C:\\Windows\\Fonts\\segoeui.ttf"),
        ("Consolas", "C:\\Windows\\Fonts\\consola.ttf"),
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert(name.to_owned(), Arc::new(FontData::from_owned(bytes)));
        }
    }
    for (name, family, front) in [
        ("SegoeUI", FontFamily::Proportional, true),
        ("Consolas", FontFamily::Monospace, true),
        ("SegoeUISymbol", FontFamily::Proportional, false),
        ("SegoeUISymbol", FontFamily::Monospace, false),
    ] {
        if fonts.font_data.contains_key(name) {
            let list = fonts.families.entry(family).or_default();
            if front {
                list.insert(0, name.to_owned());
            } else {
                list.push(name.to_owned());
            }
        }
    }
    ctx.set_fonts(fonts);
}

fn text_styles() -> std::collections::BTreeMap<TextStyle, FontId> {
    use FontFamily::{Monospace, Proportional};
    [
        (TextStyle::Heading, FontId::new(15.0, Proportional)),
        (h1(), FontId::new(15.0, Proportional)),
        (h2(), FontId::new(12.0, Proportional)),
        (TextStyle::Body, FontId::new(12.0, Proportional)),
        (TextStyle::Button, FontId::new(12.0, Proportional)),
        (TextStyle::Small, FontId::new(10.0, Proportional)),
        // Consolas at ~10pt, matching MobaXterm's default shell font.
        (TextStyle::Monospace, FontId::new(13.0, Monospace)),
    ]
    .into()
}

fn visuals() -> Visuals {
    let dark = is_dark();
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.dark_mode = dark;
    v.panel_fill = BG_PANEL();
    v.window_fill = BG_PANEL();
    v.window_stroke = Stroke::new(1.0_f32, BORDER_STRONG());
    v.window_corner_radius = CornerRadius::same(3);
    v.menu_corner_radius = CornerRadius::same(2);
    v.window_shadow = Shadow {
        offset: [0, 12],
        blur: 34,
        spread: 0,
        color: if dark {
            Color32::from_black_alpha(140)
        } else {
            Color32::from_black_alpha(64)
        },
    };
    v.popup_shadow = Shadow {
        offset: [0, 5],
        blur: 16,
        spread: 0,
        color: if dark {
            Color32::from_black_alpha(110)
        } else {
            Color32::from_black_alpha(56)
        },
    };
    v.extreme_bg_color = BG_PANEL_ALT();
    v.text_edit_bg_color = Some(BG_INPUT());
    v.code_bg_color = BG_INPUT();
    v.faint_bg_color = BG_PANEL_ALT();
    v.striped = false;
    v.hyperlink_color = ACCENT();
    v.selection.bg_fill = if dark {
        Color32::from_rgba_unmultiplied(88, 112, 250, 64)
    } else {
        Color32::from_rgba_unmultiplied(0, 120, 215, 64)
    };
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT());

    let r = CornerRadius::same(2);
    v.widgets.noninteractive = WidgetVisuals {
        bg_fill: BG_PANEL_ALT(),
        weak_bg_fill: BG_PANEL_ALT(),
        bg_stroke: Stroke::new(1.0_f32, BORDER()),
        corner_radius: CornerRadius::same(2),
        // Default label color: plain labels without an explicit color land here.
        fg_stroke: Stroke::new(1.0_f32, TEXT()),
        expansion: 0.0,
    };
    v.widgets.inactive = WidgetVisuals {
        bg_fill: BG_BTN(),
        weak_bg_fill: BG_BTN(),
        // Classic Win32 controls always carry a visible 1px edge.
        bg_stroke: Stroke::new(1.0_f32, BORDER_STRONG()),
        corner_radius: r,
        fg_stroke: Stroke::new(1.0_f32, TEXT()),
        expansion: 0.0,
    };
    v.widgets.hovered = WidgetVisuals {
        bg_fill: BG_BTN_HOVER(),
        weak_bg_fill: BG_BTN_HOVER(),
        bg_stroke: Stroke::new(1.0_f32, ACCENT()),
        corner_radius: r,
        fg_stroke: Stroke::new(1.0_f32, TEXT()),
        expansion: 0.0,
    };
    v.widgets.active = WidgetVisuals {
        bg_fill: ACCENT_SOFT_HOVER(),
        weak_bg_fill: ACCENT_SOFT_HOVER(),
        bg_stroke: Stroke::new(1.0_f32, ACCENT_HOVER()),
        corner_radius: r,
        fg_stroke: Stroke::new(1.0_f32, TEXT()),
        expansion: 0.0,
    };
    v.widgets.open = v.widgets.active;
    v
}

// ---------------------------------------------------------------------------
// Application icon (procedurally generated, no image assets required)
// ---------------------------------------------------------------------------

/// Renders the app icon: a rounded gradient tile with the Culvert mark — a
/// pipe with a stream of data flowing out of its mouth.
pub fn app_icon(size: u32) -> egui::IconData {
    let n = size as f32;
    let mut rgba = vec![0u8; (size as usize) * (size as usize) * 4];
    let top: (f32, f32, f32) = (118.0, 146.0, 255.0);
    let bottom: (f32, f32, f32) = (56.0, 85.0, 216.0);

    let cx = n * 0.5;
    let cy = n * 0.5;
    let ring_mid = n * 0.175; // pipe centreline radius
    let ring_half = n * 0.050; // half the pipe wall thickness
    let mouth = 0.40_f32; // opening half-angle, radians, facing +x
    let dash_half = n * 0.050; // stream thickness
    let pipe_color = (248.0, 250.0, 255.0);
    let stream_color = (196.0, 228.0, 255.0);

    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let half = n * 0.5 - n * 0.012;
            let d_bg = sdf_rounded_rect(px - cx, py - cy, half, half, n * 0.24);
            let a_bg = aa(d_bg);
            if a_bg <= 0.002 {
                continue;
            }

            let t = (py / n).clamp(0.0, 1.0);
            let mut r = top.0 + (bottom.0 - top.0) * t;
            let mut g = top.1 + (bottom.1 - top.1) * t;
            let mut b = top.2 + (bottom.2 - top.2) * t;

            // Pipe: a ring with a mouth cut open on the right.
            let rx = px - cx;
            let ry = py - cy;
            let rad = (rx * rx + ry * ry).sqrt().max(1.0);
            let d_ring = (rad - ring_mid).abs() - ring_half;
            // Soften the angular cut to roughly a pixel so it antialiases.
            let soft = (1.4 / rad).min(0.6);
            let ang = ry.atan2(rx).abs();
            let a_pipe = aa(d_ring) * smooth01((ang - mouth) / soft);

            // Stream: a rounded dash flowing out through the mouth.
            let d_stream =
                sdf_segment(px, py, cx + n * 0.055, cy, cx + n * 0.305, cy) - dash_half;
            let a_stream = aa(d_stream);

            let a_glyph = a_pipe.max(a_stream);
            let glyph = if a_stream > a_pipe {
                stream_color
            } else {
                pipe_color
            };
            r += (glyph.0 - r) * a_glyph;
            g += (glyph.1 - g) * a_glyph;
            b += (glyph.2 - b) * a_glyph;

            let i = ((y as usize) * size as usize + x as usize) * 4;
            rgba[i] = r.clamp(0.0, 255.0) as u8;
            rgba[i + 1] = g.clamp(0.0, 255.0) as u8;
            rgba[i + 2] = b.clamp(0.0, 255.0) as u8;
            rgba[i + 3] = (a_bg * 255.0).clamp(0.0, 255.0) as u8;
        }
    }

    egui::IconData {
        rgba,
        width: size,
        height: size,
    }
}

/// Uploads the brand mark as a texture for use inside the UI (sidebar logo).
pub fn brand_texture(ctx: &egui::Context, size: u32) -> egui::TextureHandle {
    let icon = app_icon(size);
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [size as usize, size as usize],
        &icon.rgba,
    );
    ctx.load_texture("culvert-mark", image, egui::TextureOptions::LINEAR)
}

fn aa(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

fn sdf_rounded_rect(px: f32, py: f32, hx: f32, hy: f32, r: f32) -> f32 {
    let qx = px.abs() - (hx - r);
    let qy = py.abs() - (hy - r);
    let ax = qx.max(0.0);
    let ay = qy.max(0.0);
    (ax * ax + ay * ay).sqrt() + qx.max(qy).min(0.0) - r
}

fn sdf_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let pax = px - ax;
    let pay = py - ay;
    let bax = bx - ax;
    let bay = by - ay;
    let h = ((pax * bax + pay * bay) / (bax * bax + bay * bay)).clamp(0.0, 1.0);
    let dx = pax - bax * h;
    let dy = pay - bay * h;
    (dx * dx + dy * dy).sqrt()
}

fn smooth01(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regenerates the committed brand assets from the procedural renderer.
    /// Not part of the normal test run (it writes files):
    ///     cargo test export_brand_assets -- --ignored
    #[test]
    #[ignore = "writes assets/icon.png and assets/icon.ico"]
    fn export_brand_assets() {
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
        std::fs::create_dir_all(&assets).unwrap();

        // 512 px PNG for the README and other listings.
        let icon = app_icon(512);
        let file = std::io::BufWriter::new(
            std::fs::File::create(assets.join("icon.png")).unwrap(),
        );
        let mut encoder = png::Encoder::new(file, icon.width, icon.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&icon.rgba).unwrap();
        drop(writer);

        // Multi-size ICO for the window icon and the .exe file icon.
        let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
        for size in [16u32, 32, 48, 64, 256] {
            let icon = app_icon(size);
            let image = ico::IconImage::from_rgba_data(icon.width, icon.height, icon.rgba);
            dir.add_entry(ico::IconDirEntry::encode(&image).unwrap());
        }
        let mut file = std::fs::File::create(assets.join("icon.ico")).unwrap();
        dir.write(&mut file).unwrap();

        // Sanity check: the mark must actually paint pixels.
        let icon = app_icon(64);
        let painted = icon.rgba.chunks_exact(4).filter(|p| p[3] > 0).count();
        assert!(painted > 64 * 64 / 4, "brand mark looks empty");
    }
}
