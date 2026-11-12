//! Dibujo de pantallas, modales y avisos.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Paragraph, Wrap};

use super::app::*;
use super::theme::{self, *};
use super::widgets::{self, ListState, clip, split_line};
use crate::config::{SessionRef, SetLayout};
use crate::data::graph::{Graph, Status};
use crate::data::usage::{Limit, Usage};
use crate::data::{RootKind, Session, UsageState};
use crate::{launch, paths, util};

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.buffer_mut().set_style(area, Style::new().bg(BG).fg(TEXT));
    app.hits.clear();
    app.needs_tick = false;
    app.flash_rect = None;
    if area.width < 64 || area.height < 18 {
        let p = Paragraph::new(vec![
            Line::from(Span::styled("✳ claunch", bold(ACCENT))),
            Line::from(Span::styled(format!("Agranda la terminal (mín. 64×18, ahora {}×{})", area.width, area.height), dim())),
        ]);
        f.render_widget(p, widgets::centered(area, 50, 3));
        return;
    }
    let [header, crumb, body, footer] =
        Layout::vertical([Constraint::Length(4), Constraint::Length(1), Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    draw_header(f, app, header);
    draw_crumb(f, app, crumb);
    let body = body.inner(Margin::new(1, 0));

    let mut stack = std::mem::take(&mut app.stack);
    match stack.last_mut() {
        None => draw_home(f, app, body),
        Some(Screen::Repo { key, list, filter, pick }) => draw_repo(f, app, body, key, list, filter, pick.is_some()),
        Some(Screen::PickRepo { list, filter, .. }) => draw_pick(f, app, body, list, filter),
        Some(Screen::SetEditor { set, row, name }) => draw_editor(f, app, body, *set, *row, name),
        Some(Screen::Map { sel, window }) => draw_map(f, app, body, sel, *window),
    }
    app.stack = stack;
    draw_footer(f, app, footer);

    app.fx.intro(body);
    app.fx.screen(body);
    let flash = app.flash_rect;
    app.fx.flash(flash);
    draw_toasts(f, app, area);
    if app.modal.is_some() {
        widgets::scrim(f.buffer_mut(), area);
        draw_modal(f, app, area);
    }
    app.fx.process(f.buffer_mut(), area);
}

// ---------------------------------------------------------------- marco

fn draw_header(f: &mut Frame, app: &mut App, area: Rect) {
    let shimmer = app.fx.shimmer_phase();
    let logo_area = Rect::new(area.x + 2, area.y, widgets::LOGO_WIDTH.min(area.width.saturating_sub(2)), 3);
    widgets::logo(f.buffer_mut(), logo_area, shimmer);

    let w = area.width.saturating_sub(widgets::LOGO_WIDTH + 4) as usize;
    let x = area.x + widgets::LOGO_WIDTH + 2;
    let right = |spans: Vec<Span<'static>>| {
        let width: usize = spans.iter().map(|s| s.width()).sum();
        let mut v = vec![Span::raw(" ".repeat(w.saturating_sub(width)))];
        v.extend(spans);
        Line::from(v)
    };
    let ms = app.started.elapsed().as_millis();
    let used = |v: &[Span<'static>]| v.iter().map(|s| s.width()).sum::<usize>();

    // Línea 0: aviso de bypass (prioritario) y estado: error, indexado o rótulo.
    let mut l0: Vec<Span<'static>> = Vec::new();
    let bypass_on = matches!(&app.bypass, Ok(b) if b.enabled);
    if bypass_on {
        l0.push(Span::styled(" ⚠ bypass global ", Style::new().fg(BG).bg(AMBER).add_modifier(Modifier::BOLD)));
    }
    if bypass_on && used(&l0) + 18 <= w {
        l0.push(Span::styled("  B para cambiar", faint()));
    }
    let status: Vec<Span<'static>> = match app.progress {
        Some((d, t)) if d < t => {
            let mut v = vec![Span::styled(format!("  {} indexando ", widgets::spinner(ms)), accent())];
            v.extend(widgets::progress(d, t, 14));
            v.push(Span::styled(format!(" {d}/{t}"), faint()));
            v
        }
        _ if !app.loaded => vec![Span::styled(format!("  {} cargando…", widgets::spinner(ms)), accent())],
        _ => match (&app.env, &app.cfg_error) {
            (Err(_), _) => vec![Span::styled(
                if cfg!(windows) { "  ⚠ falta claude o Git Bash" } else { "  ⚠ no encuentro claude" },
                bold(RED),
            )],
            (_, Some(_)) => vec![Span::styled("  ⚠ config.toml inválido", bold(AMBER))],
            _ => vec![Span::styled(concat!("  Claude Code launcher v", env!("CARGO_PKG_VERSION")), faint())],
        },
    };
    if !app.loaded || matches!(app.progress, Some((d, t)) if d < t) {
        app.needs_tick = true;
    }
    if used(&l0) + used(&status) <= w {
        l0.extend(status);
    }

    // Línea 1: uso del plan. Línea 2: conteos y sesiones en vivo.
    let usage = usage_spans(app, w, ms);
    let counts = count_spans(app, w, ms);
    let lines = match usage {
        Some(u) => vec![right(l0), right(u), right(counts)],
        None => vec![right(l0), right(counts), Line::raw("")],
    };
    if w > 10 {
        f.render_widget(Paragraph::new(lines), Rect::new(x, area.y, w as u16, 3));
    }
    // Filete con degradado coral → borde.
    let y = area.y + 3;
    for i in 0..area.width {
        let t = i as f32 / area.width.max(1) as f32;
        let color = theme::lerp(ACCENT, BORDER, (t * 1.6).min(1.0));
        if let Some(cell) = f.buffer_mut().cell_mut((area.x + i, y)) {
            cell.set_symbol("─").set_fg(color);
        }
    }
}

/// Conteos de la cabecera: repos, sesiones, activas, trabajando y en espera.
/// Recorta por la izquierda si no cabe (los indicadores en vivo se quedan).
fn count_spans(app: &mut App, w: usize, ms: u128) -> Vec<Span<'static>> {
    let c = &app.catalog;
    let live = app.live.len();
    let busy = app.live.values().filter(|l| l.is_busy()).count();
    let waiting = app.live.values().filter(|l| l.is_waiting()).count();
    let sep = || Span::styled("  ·  ", faint());
    let mut live_part = vec![
        Span::styled("● ", Style::new().fg(if live > 0 { GREEN } else { TEXT_FAINT })),
        Span::styled(live.to_string(), bold(TEXT)),
        Span::styled(if live == 1 { " activa" } else { " activas" }, faint()),
        sep(),
    ];
    if busy > 0 {
        app.needs_tick = true;
        live_part.push(Span::styled(format!("{} ", widgets::spinner(ms)), bold(AMBER)));
        live_part.push(Span::styled(busy.to_string(), bold(AMBER)));
        live_part.push(Span::styled(" trabajando", Style::new().fg(AMBER)));
    } else {
        live_part.push(Span::styled("✳ 0 trabajando", faint()));
    }
    if waiting > 0 {
        live_part.push(sep());
        live_part.push(Span::styled("◐ ", bold(AMBER)));
        live_part.push(Span::styled(waiting.to_string(), bold(TEXT)));
        live_part.push(Span::styled(if waiting == 1 { " espera respuesta" } else { " esperan respuesta" }, faint()));
    }
    let mut full = vec![
        Span::styled(c.repos.len().to_string(), bold(TEXT)),
        Span::styled(" repos", faint()),
        sep(),
        Span::styled(c.sessions.len().to_string(), bold(TEXT)),
        Span::styled(" sesiones", faint()),
        sep(),
    ];
    full.extend(live_part.iter().cloned());
    let width = |v: &[Span<'static>]| v.iter().map(|s| s.width()).sum::<usize>();
    if width(&full) <= w { full } else { live_part }
}

/// Barra de uso: verde < 60 %, ámbar < 85 %, rojo desde 85 %.
fn usage_bar(pct: f64, width: usize, stale: bool) -> Vec<Span<'static>> {
    let filled = ((pct / 100.0) * width as f64).round().clamp(0.0, width as f64) as usize;
    let filled = if pct > 0.0 { filled.max(1) } else { 0 };
    let color = if pct >= 85.0 { RED } else if pct >= 60.0 { AMBER } else { GREEN };
    let color = if stale { theme::lerp(color, BG, 0.45) } else { color };
    vec![Span::styled("▰".repeat(filled), Style::new().fg(color)), Span::styled("▱".repeat(width - filled), Style::new().fg(BORDER))]
}

/// «2 h 10 min», «45 min», «3 d 4 h» hasta `ms`.
fn until(ms: i64, now: i64) -> String {
    let mins = ((ms - now).max(0) + 59_999) / 60_000;
    match mins {
        0..=59 => format!("{mins} min"),
        60..=1439 => {
            let (h, m) = (mins / 60, mins % 60);
            if m == 0 { format!("{h} h") } else { format!("{h} h {m} min") }
        }
        _ => {
            let (d, h) = (mins / 1440, (mins % 1440) / 60);
            if h == 0 { format!("{d} d") } else { format!("{d} d {h} h") }
        }
    }
}

fn window_spans(label: &str, win: &Limit, bar: usize, resets: bool, now: i64, stale: bool) -> Vec<Span<'static>> {
    let mut v = vec![Span::styled(format!("{label} "), dim())];
    v.extend(usage_bar(win.pct, bar, stale));
    let pct_style = if stale { faint() } else { bold(TEXT) };
    v.push(Span::styled(format!(" {:.0}%", win.pct), pct_style));
    if resets && let Some(r) = win.resets_ms {
        v.push(Span::styled(format!(" ↻ {}", until(r, now)), faint()));
    }
    v
}

/// Línea de uso del plan, de la versión más completa a la más corta que quepa.
/// `None` si está desactivado.
fn usage_spans(app: &mut App, w: usize, ms: u128) -> Option<Vec<Span<'static>>> {
    let now = app.now_ms();
    let (u, warn): (&Usage, Option<String>) = match &app.usage {
        UsageState::Disabled => return None,
        UsageState::Loading => {
            app.needs_tick = true;
            return Some(vec![Span::styled(format!("{} consultando el uso del plan…", widgets::spinner(ms)), faint())]);
        }
        UsageState::Ready(u) => (u, None),
        UsageState::Failed { reason, last: Some(u) } => (u, Some(reason.clone())),
        UsageState::Failed { reason, last: None } => {
            // «⚠» se dibuja ancho en muchas fuentes: una columna de margen.
            let text = util::truncate(&format!("uso del plan: {reason}"), w.saturating_sub(3));
            return Some(vec![Span::styled("⚠ ", bold(AMBER)), Span::styled(text, faint())]);
        }
    };
    let stale = warn.is_some() || now - u.fetched_ms > 3 * 60_000;
    let plan = u.plan_label().map(|p| Span::styled(format!("Plan {p}   "), bold(ACCENT_SOFT)));
    let tail = |v: &mut Vec<Span<'static>>, room: usize| {
        if let Some(r) = &warn {
            let used: usize = v.iter().map(|s| s.width()).sum();
            let text = util::truncate(&format!("  ⚠ {r}"), room.saturating_sub(used));
            if util::width(&text) > 4 {
                v.push(Span::styled(text, Style::new().fg(AMBER)));
            }
        }
    };
    let head = u.headline();
    // (barra, reinicios, cuántos límites, plan)
    for (bar, resets, count, with_plan) in [
        (10, true, 2, true),
        (10, true, 2, false),
        (8, false, 2, false),
        (10, true, 1, true),
        (8, true, 1, false),
        (6, false, 1, false),
    ] {
        let mut v = Vec::new();
        if with_plan && let Some(p) = &plan {
            v.push(p.clone());
        }
        for (i, l) in head.iter().take(count).enumerate() {
            if i > 0 {
                v.push(Span::styled("   ", faint()));
            }
            v.extend(window_spans(&l.short, l, bar, resets, now, stale));
        }
        let width: usize = v.iter().map(|s| s.width()).sum();
        if width <= w {
            tail(&mut v, w);
            return Some(v);
        }
    }
    Some(vec![])
}

fn draw_crumb(f: &mut Frame, app: &App, area: Rect) {
    let parts = app.screen_name();
    let mut spans = vec![Span::raw("  ")];
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ›  ", faint()));
        }
        let last = i == parts.len() - 1;
        spans.push(Span::styled(p.clone(), if last { bold(TEXT) } else { dim() }));
    }
    let now = jiff::Zoned::now();
    const DAYS: [&str; 7] = ["lun", "mar", "mié", "jue", "vie", "sáb", "dom"];
    const MONTHS: [&str; 12] = ["ene", "feb", "mar", "abr", "may", "jun", "jul", "ago", "sep", "oct", "nov", "dic"];
    let clock = format!(
        "{} {} {} · {:02}:{:02}  ",
        DAYS[now.weekday().to_monday_zero_offset() as usize],
        now.day(),
        MONTHS[(now.month() - 1) as usize],
        now.hour(),
        now.minute()
    );
    let line = split_line(spans, vec![Span::styled(clock, faint())], area.width as usize);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let hints: Vec<(&str, &str)> = match (&app.modal, app.stack.last()) {
        (Some(Modal::Options(_)), _) => vec![("↑↓", "campo"), ("←→", "cambiar"), ("⏎", "confirmar"), ("esc", "cancelar")],
        (Some(Modal::Guard(_)), _) => vec![("f", "fork"), ("a", "abrir igual"), ("esc", "cancelar")],
        (Some(Modal::Confirm(_)), _) => vec![("s", "sí"), ("n", "no"), ("←→", "elegir"), ("⏎", "confirmar")],
        (Some(Modal::Help), _) => vec![("cualquier tecla", "cerrar")],
        (None, None) => match app.home.focus {
            Panel::Sets => vec![
                ("↑↓", "mover"), ("⏎", "abrir set"), ("e", "editar"), ("n", "nuevo"), ("d", "borrar"), ("⇥", "panel"),
                ("/", "filtrar"), ("?", "ayuda"), ("q", "salir"),
            ],
            _ => vec![
                ("↑↓", "mover"), ("⏎", "abrir"), ("o", "opciones"), ("⇥", "panel"), ("/", "filtrar"), ("m", "mapa"),
                ("n", "nuevo set"), ("B", "bypass"), ("?", "ayuda"), ("q", "salir"),
            ],
        },
        (None, Some(Screen::Repo { pick: Some(_), .. })) => vec![("↑↓", "mover"), ("⏎", "elegir"), ("/", "filtrar"), ("esc", "volver")],
        (None, Some(Screen::Repo { .. })) => {
            vec![("↑↓", "mover"), ("⏎", "abrir"), ("o", "opciones"), ("/", "filtrar"), ("r", "refrescar"), ("esc", "volver")]
        }
        (None, Some(Screen::PickRepo { .. })) => vec![("↑↓", "mover"), ("⏎", "elegir repo"), ("/", "filtrar"), ("esc", "volver")],
        (None, Some(Screen::Map { .. })) => {
            vec![("↑↓", "elegir sesión"), ("⏎", "ir al repo"), ("t", "ventana de tiempo"), ("r", "refrescar"), ("esc", "volver")]
        }
        (None, Some(Screen::SetEditor { .. })) => vec![
            ("↑↓", "mover"), ("←→", "cambiar"), ("⏎", "editar/abrir"), ("a", "añadir"), ("⇧↑↓", "reordenar"), ("d", "quitar"),
            ("esc", "volver"),
        ],
    };
    f.render_widget(Paragraph::new(widgets::key_hints(&hints, area.width)), area);
}

fn filter_title(base: &str, count: usize, filter: &Filter, focused: bool) -> Block<'static> {
    let mut block = widgets::panel(base, Some(count), focused);
    if filter.editing || !filter.text().is_empty() {
        let mut spans = vec![Span::styled(" / ", accent())];
        spans.extend(filter.input.spans(filter.editing, "filtrar…", 28));
        spans.push(Span::raw(" "));
        block = block.title_bottom(Line::from(spans));
    }
    block
}

// ---------------------------------------------------------------- filas

fn live_icon(app: &mut App, id: &str) -> Option<Span<'static>> {
    let l = app.live.get(id)?;
    if l.is_busy() {
        app.needs_tick = true;
        Some(Span::styled(format!("{} ", widgets::spinner(app.started.elapsed().as_millis())), bold(AMBER)))
    } else if l.is_waiting() {
        Some(Span::styled("◐ ", bold(AMBER)))
    } else {
        Some(Span::styled("● ", Style::new().fg(GREEN)))
    }
}

fn session_lines(app: &mut App, s: &Session, selected: bool, width: u16, show_repo: bool) -> Vec<Line<'static>> {
    let w = width as usize;
    let now = app.now_ms();
    let mut left = Vec::new();
    if let Some(icon) = live_icon(app, &s.meta.id) {
        left.push(icon);
    }
    let title_style = if selected { bold(TEXT) } else { text() };
    left.push(Span::styled(s.meta.title().to_string(), title_style));
    let time = Span::styled(util::relative_time(s.meta.mtime_ms, now), if selected { dim() } else { faint() });
    let tw = time.width() + 2;
    let l1 = split_line(clip(left, w.saturating_sub(tw)), vec![time], w);

    let sep = || Span::styled("  ·  ", Style::new().fg(BORDER));
    let mut meta: Vec<Span<'static>> = Vec::new();
    let push = |meta: &mut Vec<Span<'static>>, s: Span<'static>| {
        if !meta.is_empty() {
            meta.push(sep());
        }
        meta.push(s);
    };
    if show_repo
        && let Some(r) = app.catalog.repo(&s.repo_key) {
            push(&mut meta, Span::styled(r.name.clone(), Style::new().fg(ACCENT_SOFT)));
        }
    if !s.cwd_exists {
        push(&mut meta, Span::styled("⚠ carpeta no existe", bold(RED)));
    }
    if let Some(b) = &s.meta.git_branch {
        push(&mut meta, Span::styled(format!("⎇ {b}"), dim()));
    }
    if let Some(wt) = &s.worktree {
        push(&mut meta, Span::styled(format!("worktree {wt}"), Style::new().fg(VIOLET)));
    }
    if let Some(sub) = &s.subpath {
        push(&mut meta, Span::styled(format!("▸ {sub}"), dim()));
    }
    let (surface, color) = theme::entrypoint(s.meta.entrypoint.as_deref().unwrap_or(""));
    if !surface.is_empty() && !show_repo {
        push(&mut meta, Span::styled(surface, Style::new().fg(color)));
    }
    if !show_repo {
        push(&mut meta, Span::styled(format!("{} prompts", s.meta.prompts), faint()));
        push(&mut meta, Span::styled(util::human_size(s.meta.size), faint()));
    }
    if let Some(pr) = s.meta.pr_number {
        push(&mut meta, Span::styled(format!("PR #{pr}"), Style::new().fg(GREEN)));
    }
    let used: usize = meta.iter().map(|s| s.width()).sum();
    if let Some(p) = s.meta.last_prompt.as_deref().filter(|p| Some(*p) != s.meta.first_prompt.as_deref() || show_repo) {
        let room = w.saturating_sub(used + 5);
        if room > 12 && !show_repo {
            meta.push(sep());
            meta.push(Span::styled(
                format!("«{}»", util::truncate(p, room - 2)),
                Style::new().fg(TEXT_FAINT).add_modifier(Modifier::ITALIC),
            ));
        }
    }
    vec![l1, Line::from(clip(meta, w))]
}

fn repo_lines(app: &App, i: usize, selected: bool, width: u16) -> Vec<Line<'static>> {
    let r = &app.catalog.repos[i];
    let w = width as usize;
    let now = app.now_ms();
    let mut left = vec![Span::styled(r.name.clone(), if selected { bold(TEXT) } else { text() })];
    let live = r.sessions.iter().filter(|&&s| app.live.contains_key(&app.catalog.sessions[s].meta.id)).count();
    if live > 0 {
        left.push(Span::styled(format!("  ● {live}"), Style::new().fg(GREEN)));
    }
    if r.kind == RootKind::Folder {
        left.push(Span::styled("  carpeta", faint()));
    }
    if !r.exists {
        left.push(Span::styled("  no existe", bold(RED)));
    }
    let time = Span::styled(util::relative_time(r.last_activity_ms, now), if selected { dim() } else { faint() });
    let tw = time.width() + 2;
    let l1 = split_line(clip(left, w.saturating_sub(tw)), vec![time], w);
    let parent = r.root.parent().map(paths::display).unwrap_or_default();
    let n = r.sessions.len();
    let count = match n {
        0 => "sin sesiones".to_string(),
        1 => "1 sesión".to_string(),
        n => format!("{n} sesiones"),
    };
    let count_w = util::width(&count) + 5;
    let l2 = vec![
        Span::styled(util::truncate_left(&parent, w.saturating_sub(count_w)), faint()),
        Span::styled("  ·  ", Style::new().fg(BORDER)),
        Span::styled(count, dim()),
    ];
    vec![l1, Line::from(clip(l2, w))]
}

fn set_lines(app: &App, row: Option<usize>, selected: bool, width: u16) -> Vec<Line<'static>> {
    let w = width as usize;
    let Some(i) = row else {
        return vec![
            Line::from(Span::styled("+ Nuevo set", bold(ACCENT))),
            Line::from(clip(vec![Span::styled("Varias sesiones de golpe: pestañas, paneles o ventanas", faint())], w)),
        ];
    };
    let s = &app.cfg.sets[i];
    let n = s.entries.len();
    let right = vec![Span::styled(format!("{} · {}", s.layout.short(), n), if selected { dim() } else { faint() })];
    let rw = right.iter().map(|s| s.width()).sum::<usize>() + 2;
    let l1 = split_line(
        clip(vec![Span::styled("◆ ", accent()), Span::styled(s.name.clone(), if selected { bold(TEXT) } else { text() })], w - rw),
        right,
        w,
    );
    let names: Vec<String> = s.entries.iter().map(|e| entry_label(e, &app.catalog)).collect();
    let l2 = if names.is_empty() { "vacío: e para añadir sesiones".to_string() } else { names.join("  +  ") };
    vec![l1, Line::from(clip(vec![Span::styled(l2, faint())], w))]
}

fn register(app: &mut App, list: ListId, inner: Rect, offset: usize, item_h: u16, count: usize) {
    app.hits.push(Hit { list, rect: inner, idx: None, offset, item_h, count });
}

/// Rectángulo de la fila seleccionada (para el destello al lanzar).
fn selected_rect(inner: Rect, st: &ListState, item_h: u16) -> Option<Rect> {
    let slot = st.idx.checked_sub(st.offset)? as u16;
    let y = inner.y + slot * item_h;
    (y + item_h <= inner.bottom()).then(|| Rect::new(inner.x, y, inner.width.saturating_sub(1), item_h))
}

// ---------------------------------------------------------------- inicio

fn draw_home(f: &mut Frame, app: &mut App, area: Rect) {
    let wide = area.width >= 104;
    if wide {
        let [left, right] = Layout::horizontal([Constraint::Fill(11), Constraint::Fill(9)]).spacing(1).areas(area);
        let [top, bottom] = Layout::vertical([Constraint::Fill(3), Constraint::Fill(2)]).areas(right);
        draw_panel(f, app, Panel::Repos, left);
        draw_panel(f, app, Panel::Recents, top);
        draw_panel(f, app, Panel::Sets, bottom);
    } else {
        let [tabs, rest] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(area);
        let mut spans = vec![];
        for (i, p) in Panel::ALL.iter().enumerate() {
            let on = *p == app.home.focus;
            spans.push(Span::styled(
                format!(" {} {} ", i + 1, p.title()),
                if on { Style::new().fg(BG).bg(ACCENT).add_modifier(Modifier::BOLD) } else { dim() },
            ));
            spans.push(Span::raw(" "));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), tabs);
        let focus = app.home.focus;
        draw_panel(f, app, focus, rest);
    }
}

fn draw_panel(f: &mut Frame, app: &mut App, p: Panel, area: Rect) {
    let focused = app.home.focus == p && app.modal.is_none();
    let filter = if app.home.focus == p { app.home.filter.clone() } else { Filter::default() };
    let ft = filter.text().to_string();
    match p {
        Panel::Repos => {
            let rows = app.repo_rows(&ft);
            let keys: Vec<String> = rows.iter().map(|&i| app.catalog.repos[i].key.clone()).collect();
            app.home.repos.sync(&keys);
            let block = filter_title(p.title(), rows.len(), &filter, focused);
            f.render_widget(block, area);
            if rows.is_empty() {
                empty(f, area, if app.loaded { "Sin repos. Revisa general.scan_roots en config.toml" } else { "Buscando repos…" });
                return;
            }
            let mut st = std::mem::take(&mut app.home.repos);
            let (inner, off) = widgets::draw_list(f, area, &mut st, rows.len(), 2, |i, sel, w| repo_lines(app, rows[i], sel, w));
            if focused {
                app.flash_rect = selected_rect(inner, &st, 2);
            }
            app.home.repos = st;
            register(app, ListId::Panel(p), inner, off, 2, rows.len());
        }
        Panel::Recents => {
            let rows = app.recent_rows(&ft);
            let keys: Vec<String> = rows.iter().map(|&i| app.catalog.sessions[i].meta.id.clone()).collect();
            app.home.recents.sync(&keys);
            f.render_widget(filter_title(p.title(), rows.len(), &filter, focused), area);
            if rows.is_empty() {
                empty(f, area, if app.loaded { "Aún no hay sesiones de Claude" } else { "Cargando sesiones…" });
                return;
            }
            let mut st = std::mem::take(&mut app.home.recents);
            let sessions = app.catalog.clone();
            let (inner, off) =
                widgets::draw_list(f, area, &mut st, rows.len(), 2, |i, sel, w| session_lines(app, &sessions.sessions[rows[i]], sel, w, true));
            if focused {
                app.flash_rect = selected_rect(inner, &st, 2);
            }
            app.home.recents = st;
            register(app, ListId::Panel(p), inner, off, 2, rows.len());
        }
        Panel::Sets => {
            let rows = app.set_rows(&ft);
            let keys: Vec<String> = rows.iter().map(|r| r.map(|i| i.to_string()).unwrap_or_else(|| "+".into())).collect();
            app.home.sets.sync(&keys);
            f.render_widget(filter_title(p.title(), app.cfg.sets.len(), &filter, focused), area);
            let mut st = std::mem::take(&mut app.home.sets);
            let (inner, off) = widgets::draw_list(f, area, &mut st, rows.len(), 2, |i, sel, w| set_lines(app, rows[i], sel, w));
            if focused {
                app.flash_rect = selected_rect(inner, &st, 2);
            }
            app.home.sets = st;
            register(app, ListId::Panel(p), inner, off, 2, rows.len());
        }
    }
}

fn empty(f: &mut Frame, area: Rect, msg: &str) {
    let inner = area.inner(Margin::new(2, 1));
    f.render_widget(Paragraph::new(Span::styled(msg.to_string(), faint())).wrap(Wrap { trim: true }), inner);
}

// ---------------------------------------------------------------- repo

fn draw_repo(f: &mut Frame, app: &mut App, area: Rect, key: &str, list: &mut ListState, filter: &Filter, pick: bool) {
    let Some(repo) = app.catalog.repo(key).cloned() else {
        empty(f, area, "El repo ya no está en el índice (esc para volver)");
        return;
    };
    let [head, rest] = Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).areas(area);
    let live = repo.sessions.iter().filter(|&&s| app.live.contains_key(&app.catalog.sessions[s].meta.id)).count();
    let mut l1 = vec![Span::raw(" "), Span::styled(repo.name.clone(), bold(TEXT))];
    if repo.kind == RootKind::Folder {
        l1.push(Span::styled("  carpeta sin git", faint()));
    }
    if live > 0 {
        l1.push(Span::styled(format!("   ● {live} activa{}", if live == 1 { "" } else { "s" }), Style::new().fg(GREEN)));
    }
    let l2 = vec![Span::raw(" "), Span::styled(paths::display(&repo.root), faint())];
    f.render_widget(Paragraph::new(vec![Line::from(l1), Line::from(l2)]), head);

    let rows = app.session_rows(key, filter.text(), pick);
    list.sync(&app.sess_keys(&rows));
    let title = if pick { "Elige la sesión para el set" } else { "Sesiones" };
    let n = rows.iter().filter(|r| matches!(r, SessRow::Session(_))).count();
    f.render_widget(filter_title(title, n, filter, app.modal.is_none()), rest);
    let catalog = app.catalog.clone();
    let root = paths::display(&repo.root);
    let (inner, off) = widgets::draw_list(f, rest, list, rows.len(), 2, |i, sel, w| match rows[i] {
        SessRow::New => vec![
            Line::from(vec![Span::styled("✦ ", bold(ACCENT)), Span::styled("Nueva sesión", bold(if sel { ACCENT_SOFT } else { ACCENT }))]),
            Line::from(clip(
                vec![Span::styled(if pick { "Una sesión desde cero cada vez que abras el set".to_string() } else { format!("Claude desde cero en {root}") }, faint())],
                w as usize,
            )),
        ],
        SessRow::Latest => vec![
            Line::from(vec![Span::styled("↻ ", bold(ACCENT)), Span::styled("Última sesión de este repo", bold(if sel { ACCENT_SOFT } else { ACCENT }))]),
            Line::from(Span::styled("Se elige al abrir el set: siempre la más reciente", faint())),
        ],
        SessRow::Session(s) => session_lines(app, &catalog.sessions[s], sel, w, false),
    });
    if app.modal.is_none() {
        app.flash_rect = selected_rect(inner, list, 2);
    }
    register(app, ListId::Sessions, inner, off, 2, rows.len());
}

fn draw_pick(f: &mut Frame, app: &mut App, area: Rect, list: &mut ListState, filter: &Filter) {
    let rows = app.repo_rows(filter.text());
    let keys: Vec<String> = rows.iter().map(|&i| app.catalog.repos[i].key.clone()).collect();
    list.sync(&keys);
    f.render_widget(filter_title("Elige el repo", rows.len(), filter, app.modal.is_none()), area);
    let (inner, off) = widgets::draw_list(f, area, list, rows.len(), 2, |i, sel, w| repo_lines(app, rows[i], sel, w));
    register(app, ListId::PickRepos, inner, off, 2, rows.len());
}

// ---------------------------------------------------------------- editor de sets

fn draw_editor(f: &mut Frame, app: &mut App, area: Rect, set: usize, row: usize, name: &super::widgets::TextInput) {
    let Some(s) = app.cfg.sets.get(set).cloned() else { return };
    let rows = app.editor_rows(set);
    let wide = area.width >= 100;
    let (left, right) = if wide {
        let [l, r] = Layout::horizontal([Constraint::Fill(3), Constraint::Fill(2)]).spacing(1).areas(area);
        (l, Some(r))
    } else {
        (area, None)
    };
    let block = widgets::panel("Set", None, app.modal.is_none());
    let inner = block.inner(left).inner(Margin::new(1, 0));
    f.render_widget(block, left);

    let mut y = inner.y;
    let w = inner.width as usize;
    for (i, r) in rows.iter().enumerate() {
        let focused = i == row && app.modal.is_none();
        let h: u16 = match r {
            EdRow::Entry(_) => 2,
            EdRow::Open => 3,
            EdRow::Add => 2,
            _ => 1,
        };
        let gap_before = matches!(r, EdRow::Entry(0) | EdRow::Add | EdRow::Open) || (matches!(r, EdRow::Add) && s.entries.is_empty());
        if gap_before {
            y += 1;
        }
        if matches!(r, EdRow::Entry(0)) || (matches!(r, EdRow::Add) && s.entries.is_empty()) {
            if y < inner.bottom() {
                let label = format!("Sesiones · {}", s.entries.len());
                f.render_widget(Paragraph::new(Span::styled(label, bold(TEXT_DIM))), Rect::new(inner.x, y, inner.width, 1));
            }
            y += 1;
        }
        if y + h > inner.bottom() {
            break;
        }
        let rect = Rect::new(inner.x, y, inner.width, h);
        app.hits.push(Hit { list: ListId::SetRows, rect, idx: Some(i), offset: 0, item_h: h, count: rows.len() });
        if focused && !matches!(r, EdRow::Open) {
            f.buffer_mut().set_style(rect, Style::new().bg(SURFACE_HI));
        }
        let gutter = if focused { Span::styled("▌ ", accent()) } else { Span::raw("  ") };
        let label = |t: &'static str| Span::styled(format!("{t:<10}"), if focused { bold(ACCENT) } else { dim() });
        let cyc = |v: &str| {
            if focused {
                vec![Span::styled("‹ ", accent()), Span::styled(v.to_string(), bold(TEXT)), Span::styled(" ›", accent())]
            } else {
                vec![Span::styled(v.to_string(), text())]
            }
        };
        let lines: Vec<Line> = match r {
            EdRow::Name => {
                let mut v = vec![gutter, label("Nombre")];
                v.extend(name.spans(focused, "nombre del set", w.saturating_sub(14)));
                vec![Line::from(v)]
            }
            EdRow::Layout => {
                let mut v = vec![gutter, label("Diseño")];
                v.extend(cyc(s.layout.label()));
                vec![Line::from(v)]
            }
            EdRow::Window => {
                let mut v = vec![gutter, label("Ventana")];
                v.extend(cyc(s.window.label()));
                if s.layout == SetLayout::Windows {
                    v.push(Span::styled("  (no aplica a ventanas separadas)", faint()));
                }
                vec![Line::from(v)]
            }
            EdRow::Entry(ei) => {
                let e = &s.entries[*ei];
                let title = entry_label(e, &app.catalog);
                let color = theme::parse_hex(e.tab_color.as_deref().unwrap_or(&app.cfg.general.tab_color)).unwrap_or(ACCENT);
                let l1 = vec![
                    gutter.clone(),
                    Span::styled(format!("{} ", ei + 1), faint()),
                    Span::styled("■ ", Style::new().fg(color)),
                    Span::styled(title, if focused { bold(TEXT) } else { text() }),
                ];
                let what = match &e.session {
                    SessionRef::New => "nueva cada vez".to_string(),
                    SessionRef::Latest => "última al abrir".to_string(),
                    SessionRef::Id(id) => {
                        let ok = app.catalog.session(id).is_some();
                        format!("{}{}", &id[..id.len().min(8)], if ok { "" } else { " ⚠ no encontrada" })
                    }
                };
                let mut meta = vec![what];
                // La shell solo se elige en Windows.
                if cfg!(windows) {
                    meta.push(e.options.shell.unwrap_or(app.cfg.general.default_shell).label().to_string());
                }
                if let Some(m) = &e.options.model {
                    meta.push(m.clone());
                }
                if let Some(p) = &e.options.permission_mode {
                    meta.push(p.clone());
                }
                if e.options.fork {
                    meta.push("fork".into());
                }
                let dir = paths::display(&e.launch_dir());
                meta.push(dir);
                let l2 = vec![Span::raw(if focused { "▌   " } else { "    " }).style(accent()), Span::styled(meta.join("  ·  "), faint())];
                vec![Line::from(clip(l1, w)), Line::from(clip(l2, w))]
            }
            EdRow::Add => vec![
                Line::from(vec![gutter, Span::styled("+ Añadir sesión", bold(ACCENT))]),
                Line::from(Span::styled("    elige repo y sesión (o nueva / última)", faint())),
            ],
            EdRow::Open => {
                let n = s.entries.len();
                let label = format!("  ▶  Abrir set · {n} {}  ", if n == 1 { "sesión" } else { "sesiones" });
                let style = if focused {
                    Style::new().fg(BG).bg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(ACCENT).bg(ACCENT_BG).add_modifier(Modifier::BOLD)
                };
                let bw = util::width(&label) as u16;
                let btn = Rect::new(inner.x + 2, y + 1, bw.min(inner.width.saturating_sub(2)), 1);
                f.render_widget(Paragraph::new(Span::styled(label, style)), btn);
                if focused {
                    app.flash_rect = Some(btn);
                }
                vec![]
            }
        };
        if !lines.is_empty() {
            f.render_widget(Paragraph::new(lines), rect);
        }
        y += h;
    }
    if let Some(r) = right {
        draw_layout_preview(f, app, r, &s);
    }
}

/// Diagrama de cómo quedará el set (Windows Terminal o tmux).
fn draw_layout_preview(f: &mut Frame, app: &App, area: Rect, s: &crate::config::SessionSet) {
    let block = widgets::panel("Vista previa", None, false);
    let inner = block.inner(area).inner(Margin::new(1, 0));
    f.render_widget(block, area);
    let titles: Vec<(String, ratatui::style::Color)> = s
        .entries
        .iter()
        .map(|e| {
            let c = theme::parse_hex(e.tab_color.as_deref().unwrap_or(&app.cfg.general.tab_color)).unwrap_or(ACCENT);
            (entry_label(e, &app.catalog), c)
        })
        .collect();
    if titles.is_empty() {
        empty(f, area, "Añade sesiones para ver el diseño");
        return;
    }
    let terminal = app.env.as_ref().ok().map(|e| &e.terminal);
    let mut caption = match s.layout {
        SetLayout::Tabs => format!("{} pestañas en {}", titles.len(), s.window.label().to_lowercase()),
        SetLayout::Panes => format!("{} paneles en una pestaña ({})", titles.len(), s.window.label().to_lowercase()),
        SetLayout::Windows if cfg!(windows) => format!("{} ventanas de Windows Terminal", titles.len()),
        SetLayout::Windows => format!("{} ventanas", titles.len()),
    };
    // Sin tmux, los emuladores de Linux solo abren ventanas sueltas.
    if matches!(terminal, Some(launch::Terminal::Emulator(_))) && s.layout != SetLayout::Windows && titles.len() > 1 {
        caption.push_str(" · sin tmux se abren en ventanas separadas");
    }
    let [cap, canvas] = Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).areas(inner);
    f.render_widget(Paragraph::new(Span::styled(caption, faint())), cap);
    let canvas = Rect::new(canvas.x, canvas.y, canvas.width, canvas.height.min(16));
    let boxed = |f: &mut Frame, r: Rect, title: &str, color: ratatui::style::Color, active: bool| {
        if r.width < 4 || r.height < 2 {
            return;
        }
        let b = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if active { color } else { theme::lerp(color, SURFACE, 0.45) }))
            .style(Style::new().bg(SURFACE));
        let ir = b.inner(r);
        f.render_widget(b, r);
        if ir.height > 0 {
            let t = util::truncate(title, ir.width.saturating_sub(1) as usize);
            let y = ir.y + ir.height.saturating_sub(1) / 2;
            f.render_widget(
                Paragraph::new(Span::styled(t, if active { bold(TEXT) } else { dim() })).alignment(ratatui::layout::Alignment::Center),
                Rect::new(ir.x, y, ir.width, 1),
            );
        }
    };
    match s.layout {
        SetLayout::Panes => {
            let grid = launch::pane_grid(titles.len());
            let cols = Layout::horizontal(vec![Constraint::Fill(1); grid.len()]).split(canvas);
            let mut k = 0;
            for (c, rows) in grid.iter().enumerate() {
                let cells = Layout::vertical(vec![Constraint::Fill(1); *rows]).split(cols[c]);
                for cell in cells.iter() {
                    let (t, col) = &titles[k];
                    boxed(f, *cell, t, *col, k == 0);
                    k += 1;
                }
            }
        }
        SetLayout::Tabs => {
            let [strip, body] = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).areas(canvas);
            let mut spans = Vec::new();
            for (i, (t, c)) in titles.iter().enumerate() {
                let label = format!(" {} ", util::truncate(t, 16));
                let st = if i == 0 { Style::new().fg(BG).bg(*c).add_modifier(Modifier::BOLD) } else { Style::new().fg(*c).bg(SURFACE_HI) };
                spans.push(Span::styled(label, st));
                spans.push(Span::raw(" "));
            }
            f.render_widget(Paragraph::new(Line::from(clip(spans, strip.width as usize))), strip);
            boxed(f, body, &titles[0].0, titles[0].1, true);
        }
        SetLayout::Windows => {
            let n = titles.len().min(6) as u16;
            let bw = canvas.width.saturating_sub(3 * (n - 1)).max(10);
            let bh = canvas.height.saturating_sub(n - 1).clamp(3, 7);
            for i in (0..n).rev() {
                let r = Rect::new(canvas.x + i * 3, canvas.y + i, bw.min(canvas.width - i * 3), bh);
                f.render_widget(ratatui::widgets::Clear, r);
                let (t, c) = &titles[i as usize];
                boxed(f, r, t, *c, i == 0);
            }
        }
    }
}

// ---------------------------------------------------------------- mapa

/// Arista «caliente»: hubo un mensaje hace menos de esto (se anima).
const HOT_MS: i64 = 120_000;

fn status_icon(st: Status, ms: u128) -> (String, Style) {
    match st {
        Status::Busy => (widgets::spinner(ms).to_string(), bold(AMBER)),
        Status::Waiting => ("◐".into(), bold(AMBER)),
        Status::Idle => ("●".into(), Style::new().fg(GREEN)),
        Status::Closed => ("○".into(), faint()),
        Status::Remote => ("◇".into(), Style::new().fg(BLUE)),
        Status::Unknown => ("·".into(), faint()),
    }
}

fn status_word(st: Status) -> &'static str {
    match st {
        Status::Busy => "trabajando",
        Status::Waiting => "espera respuesta",
        Status::Idle => "inactiva",
        Status::Closed => "cerrada",
        Status::Remote => "otra máquina",
        Status::Unknown => "sin identificar",
    }
}

/// Centros de las fichas (en celdas, relativos a `area`): elipse inicial y luego
/// separación iterativa para que no se pisen. `sizes`: ancho y alto de cada ficha.
fn map_layout(sizes: &[(f64, f64)], area: Rect) -> Vec<(f64, f64)> {
    let n = sizes.len();
    let (w, h) = (area.width as f64, area.height as f64);
    let (cx, cy) = (w / 2.0, h / 2.0);
    if n == 1 {
        return vec![(cx, cy)];
    }
    let max_w = sizes.iter().map(|s| s.0).fold(0.0, f64::max);
    let rx = ((w - max_w) / 2.0 - 1.0).max(1.0);
    let ry = ((h - 2.0) / 2.0 - 0.5).max(1.0);
    let stagger = n > 10;
    let mut pos: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            let a = -std::f64::consts::FRAC_PI_2 + std::f64::consts::TAU * i as f64 / n as f64;
            let k = if stagger && i % 2 == 1 { 0.55 } else { 1.0 };
            (cx + rx * k * a.cos(), cy + ry * k * a.sin())
        })
        .collect();
    let clamp = |p: &mut (f64, f64), s: (f64, f64)| {
        p.0 = p.0.clamp(s.0 / 2.0, (w - s.0 / 2.0).max(s.0 / 2.0));
        p.1 = p.1.clamp(1.0, (h - 1.0).max(1.0));
    };
    for _ in 0..400 {
        let mut moved = false;
        for i in 0..n {
            for j in i + 1..n {
                let (dx, dy) = (pos[j].0 - pos[i].0, pos[j].1 - pos[i].1);
                // Separación mínima: medio ancho de cada una + 2 columnas; alto + 1 fila.
                let ox = (sizes[i].0 + sizes[j].0) / 2.0 + 2.0 - dx.abs();
                let oy = (sizes[i].1 + sizes[j].1) / 2.0 + 1.0 - dy.abs();
                if ox <= 0.0 || oy <= 0.0 {
                    continue;
                }
                moved = true;
                // Empuja por el eje con menos solape (en filas cuesta el doble: celdas altas).
                if oy * 2.0 < ox {
                    let d = if dy >= 0.0 { oy / 2.0 } else { -oy / 2.0 };
                    pos[i].1 -= d;
                    pos[j].1 += d;
                } else {
                    let d = if dx >= 0.0 { ox / 2.0 } else { -ox / 2.0 };
                    pos[i].0 -= d;
                    pos[j].0 += d;
                }
                clamp(&mut pos[i], sizes[i]);
                clamp(&mut pos[j], sizes[j]);
            }
        }
        if !moved {
            break;
        }
    }
    pos
}

fn draw_map(f: &mut Frame, app: &mut App, area: Rect, sel: &mut Option<String>, window: usize) {
    use ratatui::widgets::canvas::{Canvas, Line as CLine};
    let g = app.graph(window);
    let ms = app.started.elapsed().as_millis();
    let now = app.now_ms();
    let wlabel = MAP_WINDOWS[window.min(MAP_WINDOWS.len() - 1)].1;
    if sel.as_deref().and_then(|k| g.index_of(k)).is_none() {
        // Por defecto, la sesión con más mensajes.
        let degree = |i: usize| g.edges.iter().filter(|e| e.from == i || e.to == i).map(|e| e.count).sum::<u32>();
        *sel = (0..g.nodes.len()).max_by_key(|&i| (degree(i), std::cmp::Reverse(i))).map(|i| g.nodes[i].key.clone());
    }
    let si = sel.as_deref().and_then(|k| g.index_of(k));

    let wide = area.width >= 100;
    let (left, right) = if wide {
        let [l, r] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(44)]).spacing(1).areas(area);
        (l, Some(r))
    } else {
        (area, None)
    };
    let links = g.edges.len();
    let mut block = widgets::panel("Mapa de sesiones", Some(g.nodes.len()), app.modal.is_none());
    block = block.title_bottom(Line::from(vec![
        Span::styled(format!(" {links} {} ", if links == 1 { "conexión" } else { "conexiones" }), faint()),
        Span::styled(format!("· últimas {wlabel} "), faint()),
        Span::styled("t", bold(ACCENT)),
        Span::styled(" cambia ", faint()),
    ]));
    let inner = block.inner(left).inner(Margin::new(1, 0));
    f.render_widget(block, left);

    if g.nodes.is_empty() {
        empty(
            f,
            left,
            &format!(
                "No hay sesiones activas ni mensajes entre sesiones en las últimas {wlabel}. \
                 Cuando una sesión le escriba a otra (SendMessage), aparecerán aquí conectadas."
            ),
        );
        if let Some(r) = right {
            draw_map_detail(f, app, r, &g, None, now, ms, wlabel);
        }
        return;
    }

    let n = g.nodes.len();
    let max_chip = (inner.width / 3).clamp(14, 30);
    let chip_ws: Vec<u16> = g
        .nodes
        .iter()
        .map(|nd| {
            let sub = util::width(&nd.sub) + if nd.subagents > 0 { 5 } else { 0 };
            ((util::width(&nd.label).max(sub) + 5) as u16).clamp(12, max_chip).min(inner.width)
        })
        .collect();
    let sizes: Vec<(f64, f64)> = chip_ws.iter().map(|&w| (w as f64, 2.0)).collect();
    let centers = map_layout(&sizes, inner);
    let hot_any = g.edges.iter().any(|e| now - e.last_ms < HOT_MS);
    if hot_any || g.nodes.iter().any(|n| n.status == Status::Busy) {
        app.needs_tick = true;
    }

    // Aristas (braille) debajo de las fichas.
    let (cw, ch) = (inner.width as f64, inner.height as f64);
    let edge_color = |e: &crate::data::graph::Edge| {
        let age = now - e.last_ms;
        let touches = si.is_some_and(|s| e.from == s || e.to == s);
        let base = if age < HOT_MS {
            ACCENT
        } else if age < 15 * 60_000 {
            ACCENT_DEEP
        } else if age < 3_600_000 {
            theme::lerp(ACCENT_DEEP, TEXT_FAINT, 0.5)
        } else {
            TEXT_FAINT
        };
        if touches { if age < HOT_MS { HIGHLIGHT } else { theme::lerp(base, ACCENT_SOFT, 0.6) } } else { base }
    };
    let lines: Vec<CLine> = g
        .edges
        .iter()
        .map(|e| {
            let (a, b) = (centers[e.from], centers[e.to]);
            CLine::new(a.0, ch - a.1, b.0, ch - b.1, edge_color(e))
        })
        .collect();
    let canvas = Canvas::default()
        .x_bounds([0.0, cw])
        .y_bounds([0.0, ch])
        .marker(ratatui::symbols::Marker::Braille)
        .paint(|ctx| {
            for l in &lines {
                ctx.draw(l);
            }
        });
    f.render_widget(canvas, inner);

    // Paquetes en movimiento sobre las aristas calientes (del emisor al receptor).
    for e in g.edges.iter().filter(|e| now - e.last_ms < HOT_MS) {
        let (a, b) = (centers[e.from], centers[e.to]);
        for k in 0..2 {
            let t = ((ms as f64 / 1800.0) + k as f64 * 0.5).fract();
            let (x, y) = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
            let (cx, cy) = (inner.x + x.floor() as u16, inner.y + y.floor() as u16);
            if cx < inner.right() && cy < inner.bottom()
                && let Some(cell) = f.buffer_mut().cell_mut((cx, cy)) {
                    cell.set_symbol("●").set_fg(HIGHLIGHT);
                }
        }
    }

    // Fichas.
    for (i, node) in g.nodes.iter().enumerate() {
        let (x, y) = centers[i];
        let cwid = chip_ws[i];
        let left_x = (inner.x as f64 + x - cwid as f64 / 2.0).round().max(inner.x as f64) as u16;
        let left_x = left_x.min(inner.right().saturating_sub(cwid));
        let top = (inner.y as f64 + y - 0.5).round().max(inner.y as f64) as u16;
        let top = top.min(inner.bottom().saturating_sub(2));
        let rect = Rect::new(left_x, top, cwid, 2.min(inner.height));
        let selected = Some(i) == si;
        let bg = if selected { SURFACE_HI } else { SURFACE };
        f.render_widget(ratatui::widgets::Clear, rect);
        f.buffer_mut().set_style(rect, Style::new().bg(bg));
        let (icon, istyle) = status_icon(node.status, ms);
        let gutter = if selected { Span::styled("▌", accent()) } else { Span::raw(" ") };
        let tw = cwid.saturating_sub(4) as usize;
        let l1 = vec![
            gutter.clone(),
            Span::styled(format!("{icon} "), istyle),
            Span::styled(util::truncate(&node.label, tw), if selected { bold(TEXT) } else { text() }),
        ];
        let mut sub = node.sub.clone();
        if node.subagents > 0 {
            sub = format!("↳{} · {sub}", node.subagents);
        }
        let l2 = vec![gutter, Span::styled(format!("  {}", util::truncate(&sub, tw)), faint())];
        f.render_widget(Paragraph::new(vec![Line::from(l1), Line::from(l2)]).style(Style::new().bg(bg)), rect);
        app.hits.push(Hit { list: ListId::MapNodes, rect, idx: Some(i), offset: 0, item_h: 2, count: n });
    }
    if g.hidden > 0 {
        let msg = format!(" +{} sin espacio: t para acotar la ventana ", g.hidden);
        let mw = util::width(&msg) as u16;
        if mw < inner.width {
            f.render_widget(Paragraph::new(Span::styled(msg, faint())), Rect::new(inner.right() - mw, inner.y, mw, 1));
        }
    }

    match right {
        Some(r) => draw_map_detail(f, app, r, &g, si, now, ms, wlabel),
        None => {
            if let Some(i) = si {
                let node = &g.nodes[i];
                let (icon, istyle) = status_icon(node.status, ms);
                let line = Line::from(clip(
                    vec![
                        Span::styled(format!(" {icon} "), istyle),
                        Span::styled(node.label.clone(), bold(TEXT)),
                        Span::styled(format!("  ·  {}  ·  {}", status_word(node.status), node.sub), faint()),
                    ],
                    inner.width as usize,
                ));
                f.render_widget(Paragraph::new(line), Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_map_detail(f: &mut Frame, app: &App, area: Rect, g: &Graph, si: Option<usize>, now: i64, ms: u128, wlabel: &str) {
    let block = widgets::panel("Detalle", None, false);
    let inner = block.inner(area).inner(Margin::new(1, 0));
    f.render_widget(block, area);
    let w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    match si {
        None => {
            lines.push(Line::from(Span::styled("Sin sesiones que mostrar", bold(TEXT))));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled("Aquí verás las sesiones abiertas y los", dim())));
            lines.push(Line::from(Span::styled("mensajes que se mandan entre ellas.", dim())));
        }
        Some(i) => {
            let node = &g.nodes[i];
            let (icon, istyle) = status_icon(node.status, ms);
            lines.push(Line::from(clip(vec![Span::styled(format!("{icon} "), istyle), Span::styled(node.label.clone(), bold(TEXT))], w)));
            if !node.sub.is_empty() {
                lines.push(Line::from(Span::styled(util::truncate(&node.sub, w), Style::new().fg(ACCENT_SOFT))));
            }
            let mut st = status_word(node.status).to_string();
            if let Some(l) = node.session_id.as_deref().and_then(|id| app.live.get(id)) {
                if let Some(wf) = l.waiting_for.as_deref().filter(|_| l.is_waiting()) {
                    st = format!("{st} ({wf})");
                }
                let (surface, _) = theme::entrypoint(&l.entrypoint);
                if !surface.is_empty() {
                    st = format!("{st} · {surface}");
                }
                st = format!("{st} · pid {}", l.pid);
            }
            lines.push(Line::from(Span::styled(util::truncate(&st, w), dim())));
            if node.subagents > 0 {
                let s = if node.subagents == 1 { "subagente trabajando" } else { "subagentes trabajando" };
                lines.push(Line::from(Span::styled(format!("↳ {} {s}", node.subagents), Style::new().fg(VIOLET))));
            }
            if node.last_ms > 0 {
                lines.push(Line::from(Span::styled(format!("última actividad {}", util::relative_time(node.last_ms, now)), faint())));
            }
            lines.push(Line::raw(""));
            let mut mine: Vec<&crate::data::graph::Edge> = g.edges.iter().filter(|e| e.from == i || e.to == i).collect();
            mine.sort_by_key(|e| std::cmp::Reverse(e.last_ms));
            let total: u32 = mine.iter().map(|e| e.count).sum();
            lines.push(Line::from(vec![
                Span::styled("Conexiones", bold(TEXT_DIM)),
                Span::styled(format!("  {} · {total} mensajes · {wlabel}", mine.len()), faint()),
            ]));
            if mine.is_empty() {
                lines.push(Line::from(Span::styled("sin mensajes con otras sesiones", faint())));
            }
            let room = (inner.height as usize).saturating_sub(lines.len() + 5);
            for e in mine.iter().take(room.max(1)) {
                let out = e.from == i;
                let other = &g.nodes[if out { e.to } else { e.from }];
                let hot = now - e.last_ms < HOT_MS;
                let arrow = if out { "→ " } else { "← " };
                let right = format!(" {} · {}", e.count, util::relative_time(e.last_ms, now));
                let name_w = w.saturating_sub(util::width(&right) + 3);
                lines.push(split_line(
                    vec![
                        Span::styled(arrow, if hot { bold(ACCENT) } else { accent() }),
                        Span::styled(util::truncate(&other.label, name_w), if hot { bold(TEXT) } else { text() }),
                    ],
                    vec![Span::styled(right, if hot { Style::new().fg(ACCENT) } else { faint() })],
                    w,
                ));
            }
        }
    }
    f.render_widget(Paragraph::new(lines), inner);

    // Leyenda al pie.
    if inner.height >= 6 {
        let legend = vec![
            Line::from(vec![
                Span::styled("✳", bold(AMBER)),
                Span::styled(" trabajando  ", faint()),
                Span::styled("◐", bold(AMBER)),
                Span::styled(" espera  ", faint()),
                Span::styled("●", Style::new().fg(GREEN)),
                Span::styled(" inactiva", faint()),
            ]),
            Line::from(vec![
                Span::styled("○", faint()),
                Span::styled(" cerrada  ", faint()),
                Span::styled("◇", Style::new().fg(BLUE)),
                Span::styled(" otra máquina  ", faint()),
                Span::styled("↳", Style::new().fg(VIOLET)),
                Span::styled(" subagentes", faint()),
            ]),
            Line::from(vec![
                Span::styled("●⠒⠒", bold(ACCENT)),
                Span::styled(" mensaje hace < 2 min, va del emisor al receptor", faint()),
            ]),
        ];
        f.render_widget(Paragraph::new(legend), Rect::new(inner.x, inner.bottom() - 3, inner.width, 3));
    }
}

// ---------------------------------------------------------------- modales

fn draw_modal(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(modal) = app.modal.take() else { return };
    let rect = match &modal {
        Modal::Options(m) => draw_options(f, area, m),
        Modal::Guard(m) => draw_guard(f, area, m),
        Modal::Confirm(m) => draw_confirm(f, area, m),
        Modal::Help => draw_help(f, area),
    };
    app.fx.modal(rect);
    app.modal = Some(modal);
}

fn draw_options(f: &mut Frame, area: Rect, m: &OptionsModal) -> Rect {
    let h = m.fields.len() as u16 + 10;
    let rect = widgets::centered(area, 72, h);
    let title = Line::from(vec![Span::styled(" ✦ ", bold(ACCENT)), Span::styled(format!("{} ", m.heading), bold(TEXT))]);
    let inner = widgets::modal_frame(f, rect, title, ACCENT);
    let w = inner.width as usize;
    let mut lines = vec![Line::from(Span::styled(util::truncate_left(&m.sub, w), faint())), Line::raw("")];
    for (i, field) in m.fields.iter().enumerate() {
        let focused = i == m.focus;
        let mut spans = vec![
            Span::styled(if focused { "▌ " } else { "  " }, accent()),
            Span::styled(format!("{:<10}", field.label), if focused { bold(ACCENT) } else { dim() }),
        ];
        let value_w = w.saturating_sub(14);
        match &field.kind {
            FieldKind::Cycle { opts, idx } => {
                let v = opts[*idx].1.clone();
                if focused {
                    spans.push(Span::styled("‹ ", accent()));
                    spans.push(Span::styled(v, bold(TEXT)));
                    spans.push(Span::styled(" ›", accent()));
                } else {
                    spans.push(Span::styled(v, text()));
                }
                if field.id == FieldId::Color
                    && let Some(c) = theme::parse_hex(&opts[*idx].0) {
                        spans.push(Span::styled("  ■■", Style::new().fg(c)));
                    }
            }
            FieldKind::Text { input, placeholder } => {
                spans.extend(input.spans(focused, placeholder, value_w.min(36)));
            }
        }
        let hint = field.hint();
        if focused && !hint.is_empty() {
            let used: usize = spans.iter().map(|s| s.width()).sum();
            if used + 4 + util::width(hint) <= w {
                spans.push(Span::styled(format!("   {hint}"), faint()));
            }
        }
        lines.push(Line::from(clip(spans, w)));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("$ ", Style::new().fg(GREEN)),
        Span::styled(util::truncate(&m.preview(), w.saturating_sub(2)), Style::new().fg(TEXT_DIM)),
    ]));
    lines.push(Line::raw(""));
    let action = if matches!(m.purpose, OptPurpose::Entry { .. }) { "Guardar en el set" } else { "Abrir" };
    lines.push(Line::from(vec![
        Span::styled(format!("  ⏎ {action}  "), Style::new().fg(BG).bg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        Span::styled("  esc Cancelar  ", Style::new().fg(TEXT_DIM).bg(SURFACE_HI)),
    ]));
    f.render_widget(Paragraph::new(lines), inner);
    rect
}

fn draw_guard(f: &mut Frame, area: Rect, m: &GuardModal) -> Rect {
    let n = m.conflicts.len().min(4) as u16;
    let rect = widgets::centered(area, 72, 11 + n * 2 + u16::from(m.conflicts.len() > 4));
    let title = Line::from(vec![
        Span::styled(" ⚠ ", bold(AMBER)),
        Span::styled(if n > 1 { "Sesiones ya abiertas " } else { "Sesión ya abierta " }, bold(TEXT)),
    ]);
    let inner = widgets::modal_frame(f, rect, title, AMBER);
    let w = inner.width as usize;
    let mut lines = Vec::new();
    for c in m.conflicts.iter().take(4) {
        lines.push(Line::from(Span::styled(format!("«{}»", util::truncate(&c.title, w.saturating_sub(2))), bold(TEXT))));
        lines.push(Line::from(Span::styled(util::truncate(&c.detail, w), dim())));
    }
    if m.conflicts.len() > 4 {
        lines.push(Line::from(Span::styled(format!("… y {} más", m.conflicts.len() - 4), faint())));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled("Abrirla otra vez pone a dos procesos a escribir", text())));
    lines.push(Line::from(Span::styled("el mismo historial.", text())));
    lines.push(Line::raw(""));
    let opts = [("f", "Abrir como fork", "copia nueva y segura"), ("a", "Abrir igual", "bajo tu riesgo"), ("esc", "Cancelar", "")];
    for (i, (k, label, hint)) in opts.iter().enumerate() {
        let on = i == m.choice;
        lines.push(Line::from(vec![
            Span::styled(if on { "▌ " } else { "  " }, Style::new().fg(AMBER)),
            Span::styled(format!(" {k} "), Style::new().fg(TEXT).bg(SURFACE_HI)),
            Span::styled(format!("  {label}"), if on { bold(TEXT) } else { dim() }),
            Span::styled(if hint.is_empty() { String::new() } else { format!("  ·  {hint}") }, faint()),
        ]));
    }
    f.render_widget(Paragraph::new(lines), inner);
    rect
}

fn draw_confirm(f: &mut Frame, area: Rect, m: &ConfirmModal) -> Rect {
    let danger = matches!(m.action, ConfirmAction::Bypass(true) | ConfirmAction::DeleteSet(_));
    let color = if matches!(m.action, ConfirmAction::Bypass(true)) { AMBER } else if danger { RED } else { ACCENT };
    let detail_lines = (util::width(&m.detail) / 60 + 1) as u16;
    let rect = widgets::centered(area, 68, 9 + detail_lines);
    let title = Line::from(vec![Span::styled(" ● ", bold(color)), Span::styled("Confirmar ", bold(TEXT))]);
    let inner = widgets::modal_frame(f, rect, title, color);
    let [msg, detail, buttons] =
        Layout::vertical([Constraint::Length(2), Constraint::Fill(1), Constraint::Length(1)]).areas(inner);
    f.render_widget(Paragraph::new(Span::styled(m.message.clone(), bold(TEXT))).wrap(Wrap { trim: true }), msg);
    f.render_widget(Paragraph::new(Span::styled(m.detail.clone(), dim())).wrap(Wrap { trim: true }), detail);
    let yes_label = match m.action {
        ConfirmAction::DeleteSet(_) => "  s  Sí, borrar  ",
        ConfirmAction::RemoveEntry(..) => "  s  Sí, quitar  ",
        ConfirmAction::Bypass(true) => "  s  Sí, activar  ",
        ConfirmAction::Bypass(false) => "  s  Sí, desactivar  ",
    };
    let on = Style::new().fg(BG).bg(color).add_modifier(Modifier::BOLD);
    let off = Style::new().fg(TEXT_DIM).bg(SURFACE_HI);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(yes_label, if m.yes { on } else { off }),
            Span::raw("   "),
            Span::styled("  n  No  ", if m.yes { off } else { on }),
        ])),
        buttons,
    );
    rect
}

fn draw_help(f: &mut Frame, area: Rect) -> Rect {
    let rect = widgets::centered(area, 74, 30);
    let title = Line::from(vec![Span::styled(" ? ", bold(ACCENT)), Span::styled("Atajos ", bold(TEXT))]);
    let inner = widgets::modal_frame(f, rect, title, ACCENT);
    let groups: [(&str, &[(&str, &str)]); 4] = [
        ("Navegar", &[("↑↓ j k", "mover"), ("⇥ ←→ 1 2 3", "cambiar de panel"), ("⏎", "abrir / entrar"), ("esc", "volver"), ("/", "filtrar (⏎ abre)"), ("ratón", "click, doble click, rueda")]),
        (
            "Sesiones",
            &[
                ("o", if cfg!(windows) { "opciones: destino, shell, fork, modelo, permisos" } else { "opciones: destino, fork, modelo, permisos" }),
                ("r  F5", "reescanear repos y sesiones"),
            ],
        ),
        ("Sets", &[("n", "nuevo set"), ("e", "editar"), ("d", "borrar / quitar entrada"), ("a", "añadir sesión"), ("⇧↑↓", "reordenar")]),
        ("Global", &[("m", "mapa: cómo se comunican las sesiones"), ("B", "bypass global de permisos (settings.json)"), ("q  ctrl+c", "salir")]),
    ];
    let mut lines = Vec::new();
    for (g, keys) in groups {
        lines.push(Line::from(Span::styled(g, bold(ACCENT_SOFT))));
        for (k, d) in keys {
            lines.push(Line::from(vec![Span::styled(format!("  {k:<14}"), bold(TEXT)), Span::styled(d.to_string(), dim())]));
        }
        lines.push(Line::raw(""));
    }
    lines.push(Line::from(Span::styled(
        format!("Config y sets: {}", paths::display(&crate::config::path())),
        faint(),
    )));
    f.render_widget(Paragraph::new(lines), inner);
    rect
}

fn draw_toasts(f: &mut Frame, app: &mut App, area: Rect) {
    let mut y = area.bottom().saturating_sub(2);
    let mut newest = None;
    for t in app.toasts.iter().rev() {
        let (icon, color) = match t.kind {
            ToastKind::Info => ("●", ACCENT),
            ToastKind::Success => ("✓", GREEN),
            ToastKind::Warn => ("⚠", AMBER),
            ToastKind::Error => ("✕", RED),
        };
        let max = (area.width as usize).saturating_sub(10).min(76);
        let text = util::truncate(&t.text, max);
        let w = util::width(&text) as u16 + 6;
        if y < area.y + 6 {
            break;
        }
        let rect = Rect::new(area.right().saturating_sub(w + 2), y - 2, w, 3);
        f.render_widget(ratatui::widgets::Clear, rect);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(color))
            .style(Style::new().bg(SURFACE));
        let inner = block.inner(rect);
        f.render_widget(block, rect);
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(format!(" {icon} "), bold(color)), Span::styled(text, text_style())])),
            inner,
        );
        if newest.is_none() {
            newest = Some(rect);
        }
        y = y.saturating_sub(3);
    }
    if let Some(r) = newest {
        app.fx.toast(r);
    }
}

fn text_style() -> Style {
    theme::text()
}
