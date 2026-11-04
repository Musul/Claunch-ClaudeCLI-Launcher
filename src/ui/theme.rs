//! Paleta cálida inspirada en Claude, en truecolor.

use ratatui::style::{Color, Modifier, Style};

pub const BG: Color = Color::Rgb(0x17, 0x16, 0x13);
pub const SURFACE: Color = Color::Rgb(0x1f, 0x1e, 0x1a);
pub const SURFACE_HI: Color = Color::Rgb(0x2d, 0x2a, 0x24);
pub const BORDER: Color = Color::Rgb(0x38, 0x35, 0x2f);
pub const TEXT: Color = Color::Rgb(0xee, 0xea, 0xe0);
pub const TEXT_DIM: Color = Color::Rgb(0xa9, 0xa3, 0x95);
pub const TEXT_FAINT: Color = Color::Rgb(0x6e, 0x69, 0x5f);
pub const ACCENT: Color = Color::Rgb(0xd9, 0x77, 0x57);
pub const ACCENT_SOFT: Color = Color::Rgb(0xec, 0xa9, 0x8e);
pub const ACCENT_DEEP: Color = Color::Rgb(0xb5, 0x55, 0x37);
pub const ACCENT_BG: Color = Color::Rgb(0x3a, 0x26, 0x1e);
pub const HIGHLIGHT: Color = Color::Rgb(0xff, 0xf3, 0xe8);
pub const GREEN: Color = Color::Rgb(0x93, 0xc4, 0x80);
pub const AMBER: Color = Color::Rgb(0xe8, 0xb9, 0x4f);
pub const RED: Color = Color::Rgb(0xe8, 0x65, 0x5a);
pub const BLUE: Color = Color::Rgb(0x7a, 0xaa, 0xe0);
pub const VIOLET: Color = Color::Rgb(0xb9, 0x97, 0xe0);

pub fn text() -> Style {
    Style::new().fg(TEXT)
}
pub fn dim() -> Style {
    Style::new().fg(TEXT_DIM)
}
pub fn faint() -> Style {
    Style::new().fg(TEXT_FAINT)
}
pub fn accent() -> Style {
    Style::new().fg(ACCENT)
}
pub fn bold(c: Color) -> Style {
    Style::new().fg(c).add_modifier(Modifier::BOLD)
}

pub fn rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::White => (255, 255, 255),
        _ => rgb(BG),
    }
}

pub fn lerp(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let (ar, ag, ab) = rgb(a);
    let (br, bg, bb) = rgb(b);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color::Rgb(m(ar, br), m(ag, bg), m(ab, bb))
}

/// Color del punto de superficie (CLI, VS Code, Desktop) de una sesión.
pub fn entrypoint(e: &str) -> (&'static str, Color) {
    match e {
        "claude-vscode" => ("VS Code", BLUE),
        "claude-desktop" => ("Desktop", VIOLET),
        "cli" => ("CLI", TEXT_DIM),
        "sdk-cli" | "sdk-ts" | "sdk-py" => ("SDK", TEXT_DIM),
        "" => ("", TEXT_DIM),
        _ => ("otro", TEXT_DIM),
    }
}

/// Colores de pestaña ofrecidos al configurar entradas de set.
pub const TAB_COLORS: [(&str, &str); 7] = [
    ("#D97757", "coral"),
    ("#93C480", "verde"),
    ("#7AAAE0", "azul"),
    ("#E8B94F", "ámbar"),
    ("#B997E0", "violeta"),
    ("#E8655A", "rojo"),
    ("#A9A395", "gris"),
];

pub fn parse_hex(s: &str) -> Option<Color> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}
