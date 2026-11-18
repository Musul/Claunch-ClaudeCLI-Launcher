//! claunch — launcher TUI de repos y sesiones de agentes de código: Claude
//! Code, Codex, opencode, Antigravity y Pi.

mod claude_settings;
mod config;
mod data;
mod harness;
mod launch;
mod log;
mod paths;
mod setup;
mod terminal;
mod ui;
mod util;

use std::process::ExitCode;

const HELP: &str = "\
claunch — launcher de sesiones de agentes de código
          (Claude Code, Codex, opencode, Antigravity y Pi)

uso:
  claunch                       abre la TUI
  claunch open <set>            abre un set por nombre sin TUI
  claunch agents [--json | --installed]
                                agentes detectados: ejecutable, datos y sesiones
                                (--installed: solo los nombres de los instalados)
  claunch bypass [status|on|off] [--yes]
                                bypass global de permisos de Claude (~/.claude/settings.json)
  claunch usage [--json]        uso del plan de Claude (ventanas de 5 h y 7 días)
  claunch demo                  la interfaz con datos de ejemplo (para mostrarla)
";

#[cfg(windows)]
const HELP_PLATFORM: &str = "  claunch setup [--uninstall]   instala en ~/.local/bin y añade perfiles a Windows Terminal
  claunch --version

archivos:
  config y sets   %APPDATA%\\claunch\\config.toml
  índice y log    %LOCALAPPDATA%\\claunch\\
";

#[cfg(not(windows))]
const HELP_PLATFORM: &str = "  claunch setup                 instala en ~/.local/bin
  claunch --version

archivos:
  config y sets   ~/.config/claunch/config.toml
  índice y log    ~/.local/share/claunch/
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        Some("run") => launch::run_main(rest),
        Some("bypass") => claude_settings::cli(rest),
        Some("setup") => setup::main(rest),
        Some("open") => open_set(rest),
        Some("agents" | "harness" | "harnesses") => report(ui::agents(rest)),
        Some("usage") => data::usage::cli(rest),
        Some("snapshot") => report(ui::snapshot(rest)),
        Some("demo") => report(ui::run_demo()),
        Some("-h" | "--help" | "help") => {
            print!("{HELP}{HELP_PLATFORM}");
            ExitCode::SUCCESS
        }
        Some("-V" | "--version") => {
            println!("claunch {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        None => report(ui::run()),
        Some(other) => {
            eprintln!("claunch: subcomando desconocido «{other}»\n\n{HELP}{HELP_PLATFORM}");
            ExitCode::from(2)
        }
    }
}

fn report(r: anyhow::Result<()>) -> ExitCode {
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("claunch: {e:#}");
            crate::warn_log!("{e:#}");
            ExitCode::from(1)
        }
    }
}

/// `claunch open <set>`: abre un set sin pasar por la TUI (atajos, scripts).
fn open_set(args: &[String]) -> ExitCode {
    let name = args.join(" ");
    let (cfg, err) = config::Config::load();
    if let Some(e) = err {
        eprintln!("claunch: {e}");
        return ExitCode::from(1);
    }
    let Some(idx) = cfg.sets.iter().position(|s| util::fold(&s.name) == util::fold(&name)) else {
        eprintln!("claunch: no hay un set llamado «{name}». Sets: {}", cfg.sets.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", "));
        return ExitCode::from(1);
    };
    report(ui::open_set_headless(cfg, idx))
}
