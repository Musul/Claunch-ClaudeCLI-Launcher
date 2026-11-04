//! Bucle de la TUI.

pub mod app;
mod draw;
mod fx;
pub mod theme;
pub mod widgets;

use std::io::{Write, stdout};
use std::time::{Duration, Instant};

use anyhow::Context;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind};
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::crossterm::{cursor, execute};

use crate::config::Config;
use crate::data::{self, ScanConfig, WorkerMsg};
use app::App;

type Term = Terminal<CrosstermBackend<std::io::Stdout>>;

fn enter() -> anyhow::Result<()> {
    enable_raw_mode()?;
    execute!(stdout(), EnterAlternateScreen, EnableMouseCapture, cursor::Hide)?;
    // Título de la pestaña.
    print!("\x1b]0;✳ claunch\x07");
    stdout().flush()?;
    Ok(())
}

fn leave() {
    let _ = execute!(stdout(), DisableMouseCapture, LeaveAlternateScreen, cursor::Show);
    let _ = disable_raw_mode();
}

pub fn scan_config(cfg: &Config) -> ScanConfig {
    ScanConfig {
        claude_home: crate::paths::claude_home(),
        roots: cfg.scan_roots(),
        depth: cfg.general.scan_depth,
        exclude: cfg.general.scan_exclude.clone(),
    }
}

pub fn run() -> anyhow::Result<()> {
    let (cfg, cfg_error) = Config::load();
    crate::launch::cleanup_launch_dir();
    let (rx, cmd_tx) = data::spawn_worker(scan_config(&cfg));
    let mut app = App::new(cfg, cfg_error, Some(cmd_tx));

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave();
        default_hook(info);
    }));
    enter()?;
    let mut terminal: Term = Terminal::new(CrosstermBackend::new(stdout())).context("no se pudo iniciar la terminal")?;
    terminal.clear()?;
    let result = event_loop(&mut terminal, &mut app, &rx);
    leave();
    result
}

fn event_loop(terminal: &mut Term, app: &mut App, rx: &std::sync::mpsc::Receiver<WorkerMsg>) -> anyhow::Result<()> {
    let mut dirty = true;
    let mut last_draw = Instant::now();
    loop {
        while let Ok(msg) = rx.try_recv() {
            match msg {
                WorkerMsg::Progress { done, total } => app.progress = Some((done, total)),
                WorkerMsg::Catalog(c) => {
                    app.catalog = c;
                    app.loaded = true;
                    app.progress = None;
                    app.sync_lists();
                }
                WorkerMsg::Live(l) => app.live = l,
            }
            dirty = true;
        }
        app.tick();

        let animating = app.fx.running() || app.needs_tick;
        if dirty || animating || last_draw.elapsed() > Duration::from_secs(1) {
            terminal.draw(|f| draw::draw(f, app))?;
            last_draw = Instant::now();
            dirty = false;
        }
        if app.quit {
            return Ok(());
        }
        if let Some(item) = app.run_here.take() {
            run_here(terminal, app, item)?;
            dirty = true;
            continue;
        }

        let timeout = if app.fx.running() {
            Duration::from_millis(16)
        } else if app.needs_tick {
            Duration::from_millis(90)
        } else {
            Duration::from_millis(250)
        };
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => app.on_key(k),
                Event::Mouse(m) => app.on_mouse(m),
                Event::Resize(..) => {}
                _ => continue,
            }
            dirty = true;
        }
    }
}

/// Suspende la TUI, corre Claude en esta terminal y vuelve.
fn run_here(terminal: &mut Term, app: &mut App, item: crate::launch::LaunchItem) -> anyhow::Result<()> {
    let env = match &app.env {
        Ok(e) => e.clone(),
        Err(e) => {
            app.toast(app::ToastKind::Error, e.clone());
            return Ok(());
        }
    };
    leave();
    let title = item.title.clone();
    let result = env.run_here(&item);
    enter()?;
    terminal.clear()?;
    match result {
        Ok(code) => app.toast(app::ToastKind::Info, format!("Volviste de {title} (salida {code})")),
        Err(e) => app.toast(app::ToastKind::Error, format!("No se pudo abrir aquí: {e:#}")),
    }
    Ok(())
}

// ---------------------------------------------------------------- snapshot

/// `claunch snapshot <pantalla> <ancho>x<alto> <salida.html>`: renderiza con
/// datos reales a HTML para revisar el diseño en un navegador (desarrollo).
pub fn snapshot(args: &[String]) -> anyhow::Result<()> {
    use ratatui::backend::TestBackend;
    let screen = args.first().map(String::as_str).unwrap_or("home");
    let (w, h) = args
        .get(1)
        .and_then(|s| s.split_once('x'))
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
        .unwrap_or((140u16, 40u16));
    let out = args.get(2).cloned().unwrap_or_else(|| "snapshot.html".into());

    let (cfg, cfg_error) = Config::load();
    let mut app = load_blocking(cfg, cfg_error)?;
    app.toasts.clear();
    app.apply_snapshot_script(screen);
    let mut terminal = Terminal::new(TestBackend::new(w, h))?;
    terminal.draw(|f| draw::draw(f, &mut app))?;
    let html = buffer_to_html(terminal.backend().buffer());
    std::fs::write(&out, html)?;
    println!("{out}");
    Ok(())
}

/// App con el catálogo completo ya indexado (sin TUI ni animaciones).
fn load_blocking(cfg: Config, cfg_error: Option<String>) -> anyhow::Result<App> {
    let (rx, _tx) = data::spawn_worker(scan_config(&cfg));
    let mut app = App::new(cfg, cfg_error, None);
    app.fx.enabled = false;
    // El worker manda primero el índice en disco (si hay) y, tras reindexar
    // (siempre con mensajes de progreso), el catálogo completo.
    let mut saw_progress = false;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        anyhow::ensure!(Instant::now() < deadline, "el índice tardó más de 2 min");
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(WorkerMsg::Catalog(c)) => {
                app.catalog = c;
                app.loaded = true;
                if saw_progress {
                    break;
                }
            }
            Ok(WorkerMsg::Live(l)) => app.live = l,
            Ok(WorkerMsg::Progress { .. }) => saw_progress = true,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => anyhow::bail!("el indexador terminó sin catálogo"),
            Err(_) => {}
        }
    }
    app.sync_lists();
    Ok(app)
}

/// `claunch open <set>`: lanza el set y espera la confirmación de arranque.
pub fn open_set_headless(cfg: Config, idx: usize) -> anyhow::Result<()> {
    let name = cfg.sets[idx].name.clone();
    let mut app = load_blocking(cfg, None)?;
    app.toasts.clear();
    app.launch_set(idx);
    if let Some(app::Modal::Guard(g)) = &app.modal {
        for c in &g.conflicts {
            eprintln!("⚠ «{}»: {}", c.title, c.detail);
        }
        anyhow::bail!("hay sesiones del set ya abiertas; ábrelo desde la TUI para elegir fork o abrir igual");
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut shown = 0;
    loop {
        app.tick();
        for t in app.toasts.iter().skip(shown) {
            println!("{} {}", match t.kind { app::ToastKind::Success => "✓", app::ToastKind::Error => "✕", app::ToastKind::Warn => "⚠", _ => "·" }, t.text);
        }
        shown = app.toasts.len();
        if app.toasts.iter().any(|t| matches!(t.kind, app::ToastKind::Success | app::ToastKind::Error))
            || app.toasts.iter().any(|t| t.kind == app::ToastKind::Warn && t.text.contains("sin confirmar"))
        {
            break;
        }
        anyhow::ensure!(Instant::now() < deadline, "sin confirmación de arranque para «{name}»");
        std::thread::sleep(Duration::from_millis(200));
    }
    if app.toasts.iter().any(|t| t.kind == app::ToastKind::Error) {
        anyhow::bail!("no se pudo abrir «{name}»");
    }
    Ok(())
}

fn css(c: ratatui::style::Color, fallback: &str) -> String {
    match c {
        ratatui::style::Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => fallback.to_string(),
    }
}

fn buffer_to_html(buf: &ratatui::buffer::Buffer) -> String {
    use ratatui::style::Modifier;
    let bg = css(theme::BG, "#000");
    let mut s = format!(
        "<!doctype html><meta charset=utf-8><style>body{{margin:0;background:{bg}}}pre{{margin:0;padding:12px;font:15px/19px 'Cascadia Mono','Cascadia Code',Consolas,monospace;color:#eee}}span{{white-space:pre;display:inline-block;width:1ch;height:19px;overflow:visible;vertical-align:top}}span.w{{width:2ch}}</style><pre>"
    );
    let area = buf.area;
    for y in 0..area.height {
        let mut x = 0;
        while x < area.width {
            let cell = &buf[(x, y)];
            let mut style = format!("color:{};background:{}", css(cell.fg, "#eeeae0"), css(cell.bg, &bg));
            if cell.modifier.contains(Modifier::BOLD) {
                style.push_str(";font-weight:bold");
            }
            if cell.modifier.contains(Modifier::ITALIC) {
                style.push_str(";font-style:italic");
            }
            let sym = cell.symbol();
            let esc = sym.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            let cw = unicode_width::UnicodeWidthStr::width(sym).max(1) as u16;
            let class = if cw > 1 { " class=w" } else { "" };
            s.push_str(&format!("<span{class} style=\"{style}\">{esc}</span>"));
            x += cw;
        }
        s.push('\n');
    }
    s.push_str("</pre>");
    s
}
