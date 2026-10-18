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
    bg_window:          Color32::from_rgb(250, 250, 251), // #FAFAFB
    bg_panel:           Color32::from_rgb(243, 244, 246), // #F3F4F6
    bg_panel_alt:       Color32::from_rgb(238, 240, 244), // #EEF0F4
    bg_card:            Color32::from_rgb(255, 255, 255),
    bg_card_sel:        Color32::from_rgb(237, 240, 253),
    bg_input:           Color32::from_rgb(255, 255, 255),
    bg_btn:             Color32::from_rgb(255, 255, 255),
    bg_btn_hover:       Color32::from_rgb(241, 243, 247),
    border:             Color32::from_rgb(226, 229, 236),
    border_strong:      Color32::from_rgb(206, 210, 221),
    accent:             Color32::from_rgb(76, 94, 214),   // #4C5ED6
    accent_hover:       Color32::from_rgb(61, 76, 189),
    accent_soft:        Color32::from_rgb(233, 238, 254),
    accent_soft_hover:  Color32::from_rgb(219, 227, 252),
    accent_text:        Color32::from_rgb(58, 72, 180),
    text:               Color32::from_rgb(30, 33, 40),    // #1E2128
    text_dim:           Color32::from_rgb(98, 105, 118),
    text_faint:         Color32::from_rgb(148, 155, 168),
    success:            Color32::from_rgb(7, 133, 83),    // #078553
    success_bg:         Color32::from_rgb(230, 246, 238),
    warning:            Color32::from_rgb(184, 110, 8),   // #B86E08
    warning_bg:         Color32::from_rgb(253, 244, 223),
    danger:             Color32::from_rgb(211, 47, 47),   // #D32F2F
    danger_btn:         Color32::from_rgb(211, 47, 47),
    danger_btn_hover:   Color32::from_rgb(178, 34, 34),
    danger_bg:          Color32::from_rgb(252, 233, 234),
    info:               Color32::from_rgb(31, 103, 214),  // #1F67D6
    info_bg:            Color32::from_rgb(232, 240, 253),
    violet:             Color32::from_rgb(110, 58, 205),  // #6E3ACD
    violet_bg:          Color32::from_rgb(242, 236, 253),
    sb_bg:              Color32::from_rgb(27, 30, 38),    // #1B1E26
    sb_text:            Color32::from_rgb(168, 176, 192),
    sb_text_faint:      Color32::from_rgb(112, 120, 136),
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

    let mut style = (*ctx.style()).clone();
    style.visuals = visuals();
    style.text_styles = text_styles();
    {
        let s = &mut style.spacing;
        s.item_spacing = egui::vec2(7.0, 4.0);
        s.button_padding = egui::vec2(8.0, 3.0);
        s.window_margin = Margin::same(10);
        s.menu_margin = Margin::same(4);
        s.indent = 14.0;
        s.interact_size = egui::vec2(30.0, 22.0);
        s.combo_width = 120.0;
        s.tooltip_width = 420.0;
    }
    ctx.set_style(style);
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
    for (name, path) in [
        ("SegoeUISymbol", "C:\\Windows\\Fonts\\seguisym.ttf"),
        ("SegoeUI", "C:\\Windows\\Fonts\\segoeui.ttf"),
    ] {
        if let Ok(bytes) = std::fs::read(path) {
            fonts
                .font_data
                .insert(name.to_owned(), Arc::new(FontData::from_owned(bytes)));
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .push(name.to_owned());
            fonts
                .families
                .entry(FontFamily::Monospace)
                .or_default()
                .push(name.to_owned());
        }
    }
    ctx.set_fonts(fonts);
}

fn text_styles() -> std::collections::BTreeMap<TextStyle, FontId> {
    use FontFamily::{Monospace, Proportional};
    [
        (TextStyle::Heading, FontId::new(17.0, Proportional)),
        (h1(), FontId::new(18.0, Proportional)),
        (h2(), FontId::new(13.5, Proportional)),
        (TextStyle::Body, FontId::new(12.5, Proportional)),
        (TextStyle::Button, FontId::new(12.5, Proportional)),
        (TextStyle::Small, FontId::new(10.5, Proportional)),
        (TextStyle::Monospace, FontId::new(11.0, Monospace)),
    ]
    .into()
}

fn visuals() -> Visuals {
    let dark = is_dark();
    let mut v = if dark { Visuals::dark() } else { Visuals::light() };
    v.dark_mode = dark;
    v.panel_fill = BG_PANEL();
    v.window_fill = BG_CARD();
    v.window_stroke = Stroke::new(1.0_f32, BORDER_STRONG());
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
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
    v.selection.bg_fill = Color32::from_rgba_unmultiplied(88, 112, 250, 64);
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT());

    let r = CornerRadius::same(8);
    v.widgets.noninteractive = WidgetVisuals {
        bg_fill: BG_PANEL_ALT(),
        weak_bg_fill: BG_PANEL_ALT(),
        bg_stroke: Stroke::new(1.0_f32, BORDER()),
        corner_radius: CornerRadius::same(10),
        // Default label color: plain labels without an explicit color land here.
        fg_stroke: Stroke::new(1.0_f32, TEXT()),
        expansion: 0.0,
    };
    v.widgets.inactive = WidgetVisuals {
        bg_fill: BG_BTN(),
        weak_bg_fill: BG_BTN(),
        bg_stroke: Stroke::new(1.0_f32, BORDER()),
        corner_radius: r,
        fg_stroke: Stroke::new(1.0_f32, TEXT()),
        expansion: 0.0,
    };
    v.widgets.hovered = WidgetVisuals {
        bg_fill: BG_BTN_HOVER(),
        weak_bg_fill: BG_BTN_HOVER(),
        bg_stroke: Stroke::new(1.0_f32, BORDER_STRONG()),
        corner_radius: r,
        fg_stroke: Stroke::new(1.0_f32, TEXT()),
        expansion: 0.0,
    };
    v.widgets.active = WidgetVisuals {
        bg_fill: ACCENT_SOFT(),
        weak_bg_fill: ACCENT_SOFT(),
        bg_stroke: Stroke::new(1.0_f32, ACCENT()),
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
