//! Design system for the app: color palette, typography, global egui style
//! and a procedurally generated application icon.

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Shadow,
    Stroke, TextStyle, Visuals,
};
use eframe::egui::style::WidgetVisuals;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// Window / central panel background (deepest layer).
pub const BG_WINDOW: Color32 = Color32::from_rgb(8, 12, 22);
/// Side / top / bottom panel background.
pub const BG_PANEL: Color32 = Color32::from_rgb(13, 19, 34);
/// Panel variant used by the status bar and console.
pub const BG_PANEL_ALT: Color32 = Color32::from_rgb(10, 15, 28);
/// Raised card background.
pub const BG_CARD: Color32 = Color32::from_rgb(18, 26, 45);
pub const BG_CARD_HOVER: Color32 = Color32::from_rgb(23, 33, 57);
pub const BG_CARD_SEL: Color32 = Color32::from_rgb(27, 40, 70);
/// Input / console background.
pub const BG_INPUT: Color32 = Color32::from_rgb(11, 17, 31);
/// Resting button background.
pub const BG_BTN: Color32 = Color32::from_rgb(26, 36, 60);
pub const BG_BTN_HOVER: Color32 = Color32::from_rgb(35, 48, 78);

pub const BORDER: Color32 = Color32::from_rgb(31, 43, 68);
pub const BORDER_STRONG: Color32 = Color32::from_rgb(46, 62, 96);

pub const ACCENT: Color32 = Color32::from_rgb(84, 122, 255);
pub const ACCENT_SOFT: Color32 = Color32::from_rgb(37, 56, 110);
pub const ACCENT_SOFT_HOVER: Color32 = Color32::from_rgb(48, 72, 140);
pub const ACCENT_TEXT: Color32 = Color32::from_rgb(170, 190, 255);

pub const TEXT: Color32 = Color32::from_rgb(227, 233, 246);
pub const TEXT_DIM: Color32 = Color32::from_rgb(152, 166, 193);
pub const TEXT_FAINT: Color32 = Color32::from_rgb(99, 113, 141);

pub const SUCCESS: Color32 = Color32::from_rgb(61, 214, 155);
pub const SUCCESS_BG: Color32 = Color32::from_rgb(19, 54, 45);
pub const WARNING: Color32 = Color32::from_rgb(251, 191, 36);
pub const DANGER: Color32 = Color32::from_rgb(250, 120, 120);
pub const DANGER_BTN: Color32 = Color32::from_rgb(196, 58, 58);
pub const DANGER_BTN_HOVER: Color32 = Color32::from_rgb(224, 78, 78);
pub const DANGER_BG: Color32 = Color32::from_rgb(62, 25, 32);
pub const INFO: Color32 = Color32::from_rgb(125, 168, 255);
pub const INFO_BG: Color32 = Color32::from_rgb(23, 40, 78);
pub const VIOLET: Color32 = Color32::from_rgb(178, 152, 255);
pub const VIOLET_BG: Color32 = Color32::from_rgb(45, 34, 84);

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
    install_fonts(ctx);

    // The design is dark-only; pin the preference so an OS light theme cannot
    // swap in egui's default (light) style, which would drop our named styles.
    ctx.set_theme(egui::ThemePreference::Dark);

    let mut style = (*ctx.style()).clone();
    style.visuals = visuals();
    style.text_styles = text_styles();
    {
        let s = &mut style.spacing;
        s.item_spacing = egui::vec2(8.0, 8.0);
        s.button_padding = egui::vec2(13.0, 7.0);
        s.window_margin = Margin::same(14);
        s.menu_margin = Margin::same(6);
        s.indent = 16.0;
        s.interact_size = egui::vec2(40.0, 26.0);
        s.combo_width = 132.0;
        s.tooltip_width = 460.0;
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
    ctx.set_fonts(fonts);
}

fn text_styles() -> std::collections::BTreeMap<TextStyle, FontId> {
    use FontFamily::{Monospace, Proportional};
    [
        (TextStyle::Heading, FontId::new(19.0, Proportional)),
        (h1(), FontId::new(21.0, Proportional)),
        (h2(), FontId::new(15.5, Proportional)),
        (TextStyle::Body, FontId::new(14.0, Proportional)),
        (TextStyle::Button, FontId::new(14.0, Proportional)),
        (TextStyle::Small, FontId::new(12.0, Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, Monospace)),
    ]
    .into()
}

fn visuals() -> Visuals {
    let mut v = Visuals::dark();
    v.dark_mode = true;
    v.panel_fill = BG_PANEL;
    v.window_fill = BG_CARD;
    v.window_stroke = Stroke::new(1.0_f32, BORDER_STRONG);
    v.window_corner_radius = CornerRadius::same(14);
    v.menu_corner_radius = CornerRadius::same(10);
    v.window_shadow = Shadow {
        offset: [0, 10],
        blur: 30,
        spread: 0,
        color: Color32::from_black_alpha(150),
    };
    v.popup_shadow = Shadow {
        offset: [0, 6],
        blur: 20,
        spread: 0,
        color: Color32::from_black_alpha(130),
    };
    v.extreme_bg_color = BG_INPUT;
    v.text_edit_bg_color = Some(BG_INPUT);
    v.code_bg_color = BG_INPUT;
    v.faint_bg_color = BG_CARD_HOVER;
    v.striped = false;
    v.override_text_color = Some(TEXT);
    v.hyperlink_color = ACCENT_TEXT;
    v.selection.bg_fill = ACCENT_SOFT;
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);

    let r = CornerRadius::same(8);
    v.widgets.noninteractive = WidgetVisuals {
        bg_fill: BG_CARD,
        weak_bg_fill: BG_CARD,
        bg_stroke: Stroke::new(1.0_f32, BORDER),
        corner_radius: CornerRadius::same(10),
        fg_stroke: Stroke::new(1.0_f32, TEXT_DIM),
        expansion: 0.0,
    };
    v.widgets.inactive = WidgetVisuals {
        bg_fill: BG_BTN,
        weak_bg_fill: BG_BTN,
        bg_stroke: Stroke::new(1.0_f32, BORDER),
        corner_radius: r,
        fg_stroke: Stroke::new(1.0_f32, TEXT),
        expansion: 0.0,
    };
    v.widgets.hovered = WidgetVisuals {
        bg_fill: BG_BTN_HOVER,
        weak_bg_fill: BG_BTN_HOVER,
        bg_stroke: Stroke::new(1.0_f32, BORDER_STRONG),
        corner_radius: r,
        fg_stroke: Stroke::new(1.0_f32, Color32::WHITE),
        expansion: 0.0,
    };
    v.widgets.active = WidgetVisuals {
        bg_fill: ACCENT_SOFT,
        weak_bg_fill: ACCENT_SOFT,
        bg_stroke: Stroke::new(1.0_f32, ACCENT),
        corner_radius: r,
        fg_stroke: Stroke::new(1.0_f32, Color32::WHITE),
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
