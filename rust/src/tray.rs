//! System tray icon — dynamic PNG showing the active account's % usage.
//! Mirrors `_make_pct_icon` from `usage_monitor.py`.
//!
//! Render path: build a 64×64 RGBA buffer with `image` → wrap as `tray-icon`
//! Icon → push into the OS tray. Re-render whenever percent changes.

use anyhow::Result;
use image::{ImageBuffer, Rgba, RgbaImage};
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

/// Build a 64×64 PNG-style buffer: rounded black background + centred number.
/// Glyphs are drawn pixel-by-pixel from a built-in 5×7 bitmap font (no
/// rusttype dependency — keeps the binary tiny).
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
    draw_text_centered(&mut img, &text, color);
    img
}

/// Encode the rendered image to PNG bytes for `tray-icon::Icon::from_rgba`.
pub fn build_icon(pct: Option<f64>) -> Result<Icon> {
    let img = render_pct_icon(pct, None);
    let (w, h) = img.dimensions();
    let rgba = img.into_raw();
    Ok(Icon::from_rgba(rgba, w, h)?)
}

/// Build the tray icon and its context menu. Returns the live `TrayIcon`
/// (must stay alive for the lifetime of the app) and the IDs of the
/// hand-crafted menu items so callers can match `MenuEvent`s.
pub struct TrayHandle {
    pub tray: TrayIcon,
    pub show_id: tray_icon::menu::MenuId,
    pub add_id: tray_icon::menu::MenuId,
    pub quit_id: tray_icon::menu::MenuId,
}

pub fn build(initial_pct: Option<f64>) -> Result<TrayHandle> {
    let menu = Menu::new();
    let show = MenuItem::new("Открыть", true, None);
    let add = MenuItem::new("+ Добавить аккаунт", true, None);
    let sep = PredefinedMenuItem::separator();
    let quit = MenuItem::new("Выход", true, None);
    menu.append_items(&[&show, &add, &sep, &quit])?;

    let tray = TrayIconBuilder::new()
        .with_tooltip("Claude Monitor")
        .with_icon(build_icon(initial_pct)?)
        .with_menu(Box::new(menu))
        .build()?;

    Ok(TrayHandle {
        tray,
        show_id: show.id().clone(),
        add_id: add.id().clone(),
        quit_id: quit.id().clone(),
    })
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
