//! System tray icon — dynamic PNG showing the active account's % usage.
//! Mirrors `_make_pct_icon` from `usage_monitor.py`.
//!
//! Render path: build a 64×64 RGBA buffer with `image` → wrap as `tray-icon`
//! Icon → push into the OS tray. Re-render whenever percent changes.

use anyhow::Result;
use fontdue::{Font, FontSettings};
use image::{ImageBuffer, Rgba, RgbaImage};
use once_cell::sync::Lazy;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIcon, TrayIconBuilder,
};

const ICON_SIZE: u32 = 64;

/// Color for a percent value: green < 70, yellow < 90, red ≥ 90.
fn color_for(pct: f64) -> Rgba<u8> {
    if pct < 70.0 {
        Rgba([0x4a, 0xde, 0x80, 0xff])      // #4ade80
    } else if pct < 90.0 {
        Rgba([0xfa, 0xcc, 0x15, 0xff])      // #facc15
    } else {
        Rgba([0xf8, 0x71, 0x71, 0xff])      // #f87171
    }
}

/// Cyan colour the Python build uses for the weekly-limit tray icon.
pub const WEEKLY_COLOR: Rgba<u8> = Rgba([0x22, 0xd3, 0xee, 0xff]); // #22d3ee

/// System font cache. Tries Arial Bold → Segoe UI Bold → Arial. None → bitmap fallback.
static SYS_BOLD_FONT: Lazy<Option<Font>> = Lazy::new(|| {
    let candidates = [
        r"C:\Windows\Fonts\arialbd.ttf",   // Arial Bold (matches Python QFont)
        r"C:\Windows\Fonts\segoeuib.ttf",  // Segoe UI Bold
        r"C:\Windows\Fonts\arial.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(font) = Font::from_bytes(bytes, FontSettings::default()) {
                return Some(font);
            }
        }
    }
    None
});

/// Build a 64×64 PNG-style buffer: rounded black background + centred number.
/// Tries Arial Bold via fontdue; falls back to a tiny built-in 5×7 bitmap.
pub fn render_pct_icon(pct: Option<f64>, override_color: Option<Rgba<u8>>) -> RgbaImage {
    let mut img: RgbaImage = ImageBuffer::from_pixel(ICON_SIZE, ICON_SIZE, Rgba([0, 0, 0, 0]));
    draw_rounded_rect(&mut img, Rgba([0, 0, 0, 0xff]), 4);

    let (text, color) = match pct {
        None => ("C".to_string(), override_color.unwrap_or(Rgba([0x4a, 0xde, 0x80, 0xff]))),
        Some(p) => {
            let display = format!("{:.0}", p);
            (display, override_color.unwrap_or_else(|| color_for(p)))
        }
    };
    if let Some(font) = SYS_BOLD_FONT.as_ref() {
        draw_text_ttf(&mut img, font, &text, color);
    } else {
        draw_text_centered(&mut img, &text, color);
    }
    img
}

/// Render text via fontdue at a centered baseline. Composites alpha into RGBA.
fn draw_text_ttf(img: &mut RgbaImage, font: &Font, text: &str, color: Rgba<u8>) {
    // Mirror Python: 38pt for ≤2 digits, 28pt for 3. Pixel-size approx = pt * 96/72.
    let px_size: f32 = if text.chars().count() <= 2 { 50.0 } else { 38.0 };

    // First pass: total advance width and max ascent/descent.
    let metrics: Vec<_> = text.chars().map(|c| font.metrics(c, px_size)).collect();
    let total_w: f32 = metrics.iter().map(|m| m.advance_width).sum();
    let max_ascent: f32 = metrics
        .iter()
        .map(|m| (m.height as i32 + m.ymin) as f32)
        .fold(0.0_f32, f32::max);

    let start_x = ((ICON_SIZE as f32 - total_w) / 2.0).max(0.0);
    // Vertically center the bounding box of the rendered glyphs.
    let max_h: f32 = metrics.iter().map(|m| m.height as f32).fold(0.0_f32, f32::max);
    let baseline_y = (ICON_SIZE as f32 - max_h) / 2.0 + max_ascent;

    let [r, g, b, _a] = color.0;
    let mut pen_x = start_x;
    for ch in text.chars() {
        let (m, bitmap) = font.rasterize(ch, px_size);
        let glyph_left = pen_x + m.xmin as f32;
        // ymin in fontdue is the glyph's distance below the baseline (negative for descenders).
        let glyph_top = baseline_y - (m.height as f32 + m.ymin as f32);
        for y in 0..m.height {
            for x in 0..m.width {
                let alpha = bitmap[y * m.width + x];
                if alpha == 0 {
                    continue;
                }
                let px = (glyph_left as i32 + x as i32) as i32;
                let py = (glyph_top as i32 + y as i32) as i32;
                if px >= 0 && py >= 0 && (px as u32) < ICON_SIZE && (py as u32) < ICON_SIZE {
                    blend(img, px as u32, py as u32, [r, g, b, alpha]);
                }
            }
        }
        pen_x += m.advance_width;
    }
}

/// Source-over alpha blend a single pixel.
fn blend(img: &mut RgbaImage, x: u32, y: u32, src: [u8; 4]) {
    let dst = img.get_pixel(x, y).0;
    let sa = src[3] as u32;
    let inv = 255 - sa;
    let r = ((src[0] as u32 * sa + dst[0] as u32 * inv) / 255) as u8;
    let g = ((src[1] as u32 * sa + dst[1] as u32 * inv) / 255) as u8;
    let b = ((src[2] as u32 * sa + dst[2] as u32 * inv) / 255) as u8;
    let a = (sa + dst[3] as u32 * inv / 255).min(255) as u8;
    img.put_pixel(x, y, Rgba([r, g, b, a]));
}

/// Encode the rendered image to PNG bytes for `tray-icon::Icon::from_rgba`.
pub fn build_icon(pct: Option<f64>) -> Result<Icon> {
    build_icon_with_color(pct, None)
}

/// Build an Icon with an optional fixed colour (used for the weekly cyan icon).
pub fn build_icon_with_color(pct: Option<f64>, override_color: Option<Rgba<u8>>) -> Result<Icon> {
    let img = render_pct_icon(pct, override_color);
    let (w, h) = img.dimensions();
    let rgba = img.into_raw();
    Ok(Icon::from_rgba(rgba, w, h)?)
}

/// Build the tray icon and its context menu. Returns the live `TrayIcon`
/// (must stay alive for the lifetime of the app) and the IDs of the
/// hand-crafted menu items so callers can match `MenuEvent`s.
pub struct TrayHandle {
    pub session: TrayIcon,                        // 5h session icon (traffic-light)
    pub weekly: TrayIcon,                         // 7d weekly icon (always cyan)
    pub show_id: tray_icon::menu::MenuId,
    pub add_id: tray_icon::menu::MenuId,
    pub quit_id: tray_icon::menu::MenuId,
}

pub fn build(session_pct: Option<f64>, weekly_pct: Option<f64>) -> Result<TrayHandle> {
    let menu = Menu::new();
    let show = MenuItem::new("Открыть", true, None);
    let add = MenuItem::new("+ Добавить аккаунт", true, None);
    let sep = PredefinedMenuItem::separator();
    let quit = MenuItem::new("Выход", true, None);
    menu.append_items(&[&show, &add, &sep, &quit])?;

    // Both icons share the SAME menu (right-click on either opens the same).
    let session = TrayIconBuilder::new()
        .with_tooltip("5-часовая сессия")
        .with_icon(build_icon(session_pct)?)
        .with_menu(Box::new(clone_menu(&menu, &show, &add, &sep, &quit)?))
        .with_menu_on_left_click(false)
        .build()?;

    let weekly = TrayIconBuilder::new()
        .with_tooltip("7-дневный лимит")
        .with_icon(build_icon_with_color(weekly_pct, Some(WEEKLY_COLOR))?)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()?;

    Ok(TrayHandle {
        session,
        weekly,
        show_id: show.id().clone(),
        add_id: add.id().clone(),
        quit_id: quit.id().clone(),
    })
}

/// tray-icon `Menu` items can't be shared between two TrayIcons (each tray
/// takes ownership of the Menu). We build a second menu instance with the
/// same labels but separate items — the IDs are still tracked from the
/// originals so dispatch works for the cyan tray's menu too.
fn clone_menu(
    _orig: &Menu,
    _show: &MenuItem,
    _add: &MenuItem,
    _sep: &PredefinedMenuItem,
    _quit: &MenuItem,
) -> Result<Menu> {
    let m = Menu::new();
    let s = MenuItem::with_id(_show.id().clone(), "Открыть", true, None);
    let a = MenuItem::with_id(_add.id().clone(), "+ Добавить аккаунт", true, None);
    let p = PredefinedMenuItem::separator();
    let q = MenuItem::with_id(_quit.id().clone(), "Выход", true, None);
    m.append_items(&[&s, &a, &p, &q])?;
    Ok(m)
}

/// Update the session tray icon and tooltip.
/// `reset_text` is the pre-formatted "Xч Yм" / "Xд Yч" string from
/// `format_remaining`; empty means "no reset data available".
pub fn update_session(
    handle: &TrayHandle,
    pct: Option<f64>,
    reset_text: &str,
) -> Result<()> {
    handle.session.set_icon(Some(build_icon(pct)?))?;
    let tip = if reset_text.is_empty() {
        "5-часовая сессия".to_string()
    } else {
        format!("5-часовая сессия — {reset_text} до сброса")
    };
    handle.session.set_tooltip(Some(tip))?;
    Ok(())
}

/// Update the weekly tray icon (always cyan) and tooltip.
pub fn update_weekly(
    handle: &TrayHandle,
    pct: Option<f64>,
    reset_text: &str,
) -> Result<()> {
    handle
        .weekly
        .set_icon(Some(build_icon_with_color(pct, Some(WEEKLY_COLOR))?))?;
    let tip = if reset_text.is_empty() {
        "7-дневный лимит".to_string()
    } else {
        format!("7-дневный лимит — {reset_text} до сброса")
    };
    handle.weekly.set_tooltip(Some(tip))?;
    Ok(())
}

/// Pump menu events into a channel. Call once at startup. The receiver
/// converts them into UI commands.
pub fn forward_menu_events(tx: tokio::sync::mpsc::Sender<MenuEvent>) {
    let receiver = MenuEvent::receiver();
    std::thread::spawn(move || {
        while let Ok(ev) = receiver.recv() {
            if tx.blocking_send(ev).is_err() {
                break;
            }
        }
    });
}

// ─────────────────────────────── primitives ──────────────────────────────────

fn draw_rounded_rect(img: &mut RgbaImage, color: Rgba<u8>, radius: i32) {
    let w = img.width() as i32;
    let h = img.height() as i32;
    for y in 0..h {
        for x in 0..w {
            let in_corner = corner_outside(x, y, radius, w, h);
            if !in_corner {
                img.put_pixel(x as u32, y as u32, color);
            }
        }
    }
}

fn corner_outside(x: i32, y: i32, r: i32, w: i32, h: i32) -> bool {
    let (cx, cy) = if x < r && y < r {
        (r, r)
    } else if x >= w - r && y < r {
        (w - r - 1, r)
    } else if x < r && y >= h - r {
        (r, h - r - 1)
    } else if x >= w - r && y >= h - r {
        (w - r - 1, h - r - 1)
    } else {
        return false;
    };
    let dx = x - cx;
    let dy = y - cy;
    dx * dx + dy * dy > r * r
}

// ─────────────────────────────── tiny pixel font ─────────────────────────────
//
// 5×7 glyphs for digits 0-9 and the letter "C". Each row is 5 bits, MSB = leftmost.
// Lifted from a public-domain bitmap font, hand-tuned for legibility at this size.
fn glyph_for(c: char) -> Option<[u8; 7]> {
    match c {
        '0' => Some([0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110]),
        '1' => Some([0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110]),
        '2' => Some([0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111]),
        '3' => Some([0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110]),
        '4' => Some([0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010]),
        '5' => Some([0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110]),
        '6' => Some([0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110]),
        '7' => Some([0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000]),
        '8' => Some([0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110]),
        '9' => Some([0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100]),
        'C' => Some([0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110]),
        _ => None,
    }
}

fn draw_text_centered(img: &mut RgbaImage, text: &str, color: Rgba<u8>) {
    // Glyphs are 5 wide; we scale to keep the icon readable.
    let scale: u32 = if text.len() <= 2 { 7 } else { 5 };
    let glyph_w = 5 * scale;
    let glyph_h = 7 * scale;
    let spacing = scale; // 1 px gap × scale
    let total_w = (text.chars().count() as u32) * glyph_w
        + spacing * (text.chars().count().saturating_sub(1) as u32);
    let mut x_cursor = (ICON_SIZE.saturating_sub(total_w)) / 2;
    let y_top = (ICON_SIZE.saturating_sub(glyph_h)) / 2;

    for ch in text.chars() {
        if let Some(rows) = glyph_for(ch) {
            for (row, bits) in rows.iter().enumerate() {
                for col in 0..5 {
                    if (bits >> (4 - col)) & 1 == 1 {
                        for sx in 0..scale {
                            for sy in 0..scale {
                                let px = x_cursor + col * scale + sx;
                                let py = y_top + (row as u32) * scale + sy;
                                if px < ICON_SIZE && py < ICON_SIZE {
                                    img.put_pixel(px, py, color);
                                }
                            }
                        }
                    }
                }
            }
        }
        x_cursor += glyph_w + spacing;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_some_pixels_for_75pct() {
        let img = render_pct_icon(Some(75.0), None);
        assert_eq!(img.dimensions(), (ICON_SIZE, ICON_SIZE));
        // At least some pixels must be non-transparent (background + text).
        let opaque = img.pixels().filter(|p| p.0[3] > 0).count();
        assert!(opaque > 100, "expected painted pixels, got {opaque}");
    }

    #[test]
    fn placeholder_glyph_for_none() {
        let img = render_pct_icon(None, None);
        // "C" pixel should appear roughly centred.
        let opaque = img.pixels().filter(|p| p.0[3] > 0).count();
        assert!(opaque > 50);
    }
}
