//! Piezas visuales reutilizables.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use super::theme::{self, *};
use crate::util;

// ---------------------------------------------------------------- logo

/// Fuente de 6 px de alto que se pinta en 3 filas con medios bloques.
const GLYPHS: &[(char, [&str; 6])] = &[
    ('*', ["..#..", "#.#.#", ".###.", ".###.", "#.#.#", "..#.."]),
    ('c', ["....", "....", ".###", "#...", "#...", ".###"]),
    ('l', ["#.", "#.", "#.", "#.", "#.", ".#"]),
    ('a', [".....", ".....", ".####", "#...#", "#...#", ".####"]),
    ('u', ["....", "....", "#..#", "#..#", "#..#", ".###"]),
    ('n', ["....", "....", "###.", "#..#", "#..#", "#..#"]),
    ('h', ["#...", "#...", "###.", "#..#", "#..#", "#..#"]),
];

pub const LOGO_WIDTH: u16 = 40;

/// Logo `✳ claunch` con degradado y un brillo que recorre las letras
/// mientras `shimmer` está en (0, 1).
pub fn logo(buf: &mut Buffer, area: Rect, shimmer: Option<f32>) {
    let word = "*claunch";
    let mut columns: Vec<(bool, [bool; 6])> = Vec::new();
    for (i, ch) in word.chars().enumerate() {
        let Some((_, g)) = GLYPHS.iter().find(|(c, _)| *c == ch) else { continue };
        let w = g[0].len();
        for x in 0..w {
            let mut col = [false; 6];
            for (y, row) in g.iter().enumerate() {
                col[y] = row.as_bytes()[x] == b'#';
            }
            columns.push((i == 0, col));
        }
        columns.push((i == 0, [false; 6]));
        if i == 0 {
            columns.push((true, [false; 6]));
        }
    }
    let total = columns.len() as f32;
    for (x, (is_mark, col)) in columns.iter().enumerate() {
        let cx = area.x + x as u16;
        if cx >= area.right() {
            break;
        }
        let t = x as f32 / total;
        let mut color = if *is_mark { ACCENT } else { theme::lerp(ACCENT_SOFT, ACCENT_DEEP, t) };
        if let Some(s) = shimmer {
            // Banda brillante que avanza de izquierda a derecha.
            let pos = -0.2 + s * 1.4;
            let d = (t - pos).abs();
            if d < 0.12 {
                color = theme::lerp(color, HIGHLIGHT, 1.0 - d / 0.12);
            }
        }
        for row in 0..3u16 {
            let cy = area.y + row;
            if cy >= area.bottom() {
                break;
            }
            let (top, bot) = (col[row as usize * 2], col[row as usize * 2 + 1]);
            let sym = match (top, bot) {
                (true, true) => "█",
                (true, false) => "▀",
                (false, true) => "▄",
                (false, false) => continue,
            };
            if let Some(cell) = buf.cell_mut((cx, cy)) {
                cell.set_symbol(sym).set_fg(color);
            }
        }
    }
}

// ---------------------------------------------------------------- textos

pub const SPINNER: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];

/// Spinner estilo Claude Code (ida y vuelta).
pub fn spinner(ms: u128) -> &'static str {
    let n = SPINNER.len() as u128;
    let i = (ms / 110) % (2 * n - 2);
    SPINNER[if i < n { i } else { 2 * n - 2 - i } as usize]
}

/// Barra de progreso fina: `▰▰▰▱▱`.
pub fn progress(done: usize, total: usize, width: usize) -> Vec<Span<'static>> {
    let filled = if total == 0 { 0 } else { (done * width).div_ceil(total).min(width) };
    vec![
        Span::styled("▰".repeat(filled), accent()),
        Span::styled("▱".repeat(width - filled), Style::new().fg(BORDER)),
    ]
}

/// Atajos de teclado del pie: tecla resaltada + descripción tenue.
pub fn key_hints(hints: &[(&str, &str)], width: u16) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (k, label) in hints {
        let w = util::width(k) + util::width(label) + 4;
        if used + w > width as usize {
            break;
        }
        spans.push(Span::styled(format!(" {k} "), Style::new().fg(TEXT).bg(SURFACE_HI)));
        spans.push(Span::styled(format!(" {label}  "), faint()));
        used += w;
    }
    Line::from(spans)
}

/// Línea con texto a la izquierda y a la derecha, rellenando entre medias.
pub fn split_line(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let lw: usize = left.iter().map(|s| s.width()).sum();
    let rw: usize = right.iter().map(|s| s.width()).sum();
    let mut spans = left;
    if lw + rw < width {
        spans.push(Span::raw(" ".repeat(width - lw - rw)));
        spans.extend(right);
    }
    Line::from(spans)
}

/// Recorta una lista de spans a `max` columnas.
pub fn clip(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for s in spans {
        let w = s.width();
        if used + w <= max {
            used += w;
            out.push(s);
        } else {
            let rest = max.saturating_sub(used);
            if rest > 0 {
                out.push(Span::styled(util::truncate(&s.content, rest), s.style));
            }
            break;
        }
    }
    out
}

// ---------------------------------------------------------------- paneles

pub fn panel(title: &str, count: Option<usize>, focused: bool) -> Block<'static> {
    let border = if focused { ACCENT } else { BORDER };
    let mut t = vec![Span::styled(format!(" {title} "), if focused { bold(ACCENT) } else { bold(TEXT_DIM) })];
    if let Some(n) = count {
        t.push(Span::styled(format!("{n} "), faint()));
    }
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .style(Style::new().bg(SURFACE))
        .title(Line::from(t))
}

/// Estado de una lista con selección estable por clave.
#[derive(Debug, Default, Clone)]
pub struct ListState {
    pub idx: usize,
    pub offset: usize,
    pub key: Option<String>,
}

impl ListState {
    /// Reubica la selección en el elemento con la misma clave tras un refresco.
    pub fn sync(&mut self, keys: &[String]) {
        if keys.is_empty() {
            self.idx = 0;
            self.key = None;
            return;
        }
        if let Some(k) = &self.key
            && let Some(i) = keys.iter().position(|x| x == k) {
                self.idx = i;
            }
        self.idx = self.idx.min(keys.len() - 1);
        self.key = Some(keys[self.idx].clone());
    }

    pub fn select(&mut self, idx: usize, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        self.idx = idx.min(keys.len() - 1);
        self.key = Some(keys[self.idx].clone());
    }

    pub fn step(&mut self, delta: isize, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let n = keys.len() as isize;
        let i = (self.idx as isize + delta).clamp(0, n - 1);
        self.select(i as usize, keys);
    }
}

/// Dibuja una lista de elementos de `item_h` filas; devuelve el área interior
/// y el offset usado (para el ratón).
pub fn draw_list(
    f: &mut Frame,
    area: Rect,
    state: &mut ListState,
    count: usize,
    item_h: u16,
    mut render: impl FnMut(usize, bool, u16) -> Vec<Line<'static>>,
) -> (Rect, usize) {
    let inner = area.inner(Margin::new(1, 1));
    if inner.height == 0 || inner.width < 4 {
        return (inner, 0);
    }
    let visible = (inner.height / item_h).max(1) as usize;
    if state.idx < state.offset {
        state.offset = state.idx;
    } else if state.idx >= state.offset + visible {
        state.offset = state.idx + 1 - visible;
    }
    state.offset = state.offset.min(count.saturating_sub(visible));
    let row_w = inner.width.saturating_sub(1);
    for (slot, i) in (state.offset..count.min(state.offset + visible)).enumerate() {
        let selected = i == state.idx;
        let y = inner.y + slot as u16 * item_h;
        let rect = Rect::new(inner.x, y, row_w, item_h.min(inner.bottom() - y));
        let bg = if selected { SURFACE_HI } else { SURFACE };
        f.buffer_mut().set_style(rect, Style::new().bg(bg));
        let gutter = if selected { Span::styled("▌", accent()) } else { Span::raw(" ") };
        let lines = render(i, selected, row_w.saturating_sub(3));
        for (r, line) in lines.into_iter().take(item_h as usize).enumerate() {
            let mut spans = vec![if r == 0 || selected { gutter.clone() } else { Span::raw(" ") }, Span::raw(" ")];
            spans.extend(line.spans);
            f.render_widget(Paragraph::new(Line::from(spans)), Rect::new(rect.x, rect.y + r as u16, rect.width, 1));
        }
    }
    if count > visible {
        let mut sb = ScrollbarState::new(count.saturating_sub(visible)).position(state.offset);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .track_style(Style::new().fg(BORDER))
                .thumb_symbol("┃")
                .thumb_style(Style::new().fg(TEXT_FAINT)),
            area.inner(Margin::new(0, 1)),
            &mut sb,
        );
    }
    (inner, state.offset)
}

// ---------------------------------------------------------------- input

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    pub value: String,
    /// Posición del cursor en caracteres.
    pub cursor: usize,
}

impl TextInput {
    pub fn new(value: &str) -> Self {
        TextInput { value: value.to_string(), cursor: value.chars().count() }
    }

    fn byte(&self, ch: usize) -> usize {
        self.value.char_indices().nth(ch).map(|(b, _)| b).unwrap_or(self.value.len())
    }

    /// Procesa una tecla; `true` si la consumió.
    pub fn handle(&mut self, key: &ratatui::crossterm::event::KeyEvent) -> bool {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers};
        let len = self.value.chars().count();
        match key.code {
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.value.clear();
                self.cursor = 0;
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let before: String = self.value.chars().take(self.cursor).collect();
                let trimmed = before.trim_end();
                let cut = trimmed.rfind(' ').map(|i| i + 1).unwrap_or(0);
                let keep = trimmed[..cut].chars().count();
                let start = self.byte(keep);
                let end = self.byte(self.cursor);
                self.value.replace_range(start..end, "");
                self.cursor = keep;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let b = self.byte(self.cursor);
                self.value.insert(b, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let b = self.byte(self.cursor - 1);
                self.value.remove(b);
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < len => {
                let b = self.byte(self.cursor);
                self.value.remove(b);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }

    /// Spans con cursor de bloque si `focused`; `placeholder` si está vacío.
    pub fn spans(&self, focused: bool, placeholder: &str, width: usize) -> Vec<Span<'static>> {
        if self.value.is_empty() {
            let mut v = Vec::new();
            if focused {
                v.push(Span::styled(" ", Style::new().bg(ACCENT)));
            }
            v.push(Span::styled(util::truncate(placeholder, width.saturating_sub(1)), faint()));
            return v;
        }
        let chars: Vec<char> = self.value.chars().collect();
        // Desplaza la ventana para que el cursor quede visible.
        let start = self.cursor.saturating_sub(width.saturating_sub(2));
        let before: String = chars[start..self.cursor].iter().collect();
        let at: String = chars.get(self.cursor).map(|c| c.to_string()).unwrap_or_else(|| " ".into());
        let after: String = chars.get(self.cursor + 1..).map(|s| s.iter().collect()).unwrap_or_default();
        if !focused {
            return vec![Span::styled(util::truncate(&self.value, width), text())];
        }
        let room = width.saturating_sub(util::width(&before) + 1);
        vec![
            Span::styled(before, text()),
            Span::styled(at, Style::new().fg(BG).bg(ACCENT)),
            Span::styled(util::truncate(&after, room), text()),
        ]
    }
}

// ---------------------------------------------------------------- modal

/// Oscurece todo lo ya dibujado (fondo de modal).
pub fn scrim(buf: &mut Buffer, area: Rect) {
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                let fg = theme::lerp(cell.fg, BG, 0.65);
                let bg = theme::lerp(cell.bg, Color::Black, 0.35);
                cell.set_fg(fg).set_bg(bg);
                cell.modifier.remove(Modifier::BOLD);
            }
        }
    }
}

/// Rectángulo centrado de `w`×`h` dentro de `area`.
pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2));
    let h = h.min(area.height.saturating_sub(2));
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

pub fn modal_frame(f: &mut Frame, rect: Rect, title: Line<'static>, color: Color) -> Rect {
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .style(Style::new().bg(SURFACE).fg(TEXT))
        .title(title);
    let inner = block.inner(rect).inner(Margin::new(2, 1));
    f.render_widget(block, rect);
    inner
}
