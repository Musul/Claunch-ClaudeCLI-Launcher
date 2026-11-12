//! Dónde abrir sesiones en Linux: tmux o un emulador de terminal gráfico.
//!
//! - Dentro de tmux (`$TMUX`): cada sesión es una ventana de tmux y un set en
//!   paneles se reparte en una sola ventana con diseño `tiled`.
//! - Fuera de tmux, con escritorio gráfico: una ventana del emulador por sesión
//!   (no hay una forma común a todos de abrir pestañas o paneles).
//! - Sin ninguno de los dos (SSH sin tmux): solo «Esta terminal».
//!
//! `general.terminal` fija el emulador (o `["tmux"]`). La detección y los
//! argumentos son funciones puras: se compilan y prueban también en Windows.

#![cfg_attr(windows, allow(dead_code))]

use crate::config::SetLayout;
use crate::launch::{LaunchItem, Placement, Terminal};

/// Emuladores conocidos, en orden de preferencia, y los argumentos que van
/// antes del comando. `x-terminal-emulator` (Debian/Ubuntu) apunta al
/// emulador elegido en el sistema y siempre acepta `-e`.
const KNOWN: [(&str, &[&str]); 12] = [
    ("x-terminal-emulator", &["-e"]),
    ("gnome-terminal", &["--"]),
    ("konsole", &["-e"]),
    ("xfce4-terminal", &["-x"]),
    ("mate-terminal", &["-x"]),
    ("kitty", &[]),
    ("alacritty", &["-e"]),
    ("wezterm", &["start", "--"]),
    ("ghostty", &["-e"]),
    ("foot", &[]),
    ("terminator", &["-x"]),
    ("xterm", &["-e"]),
];

/// Argumentos de un emulador conocido (por nombre o ruta), o `-e` si no se conoce.
fn exec_args(program: &str) -> Vec<String> {
    let name = program.rsplit('/').next().unwrap_or(program);
    KNOWN
        .iter()
        .find(|(n, _)| *n == name)
        .map_or_else(|| vec!["-e".to_string()], |(_, a)| a.iter().map(|s| s.to_string()).collect())
}

/// Emulador propio del escritorio (`XDG_CURRENT_DESKTOP`, p. ej. `ubuntu:GNOME`).
fn desktop_terminal(desktop: &str) -> Option<&'static str> {
    let parts: Vec<String> = desktop.split(':').map(|p| p.trim().to_ascii_uppercase()).collect();
    let has = |x: &str| parts.iter().any(|p| p == x || p.ends_with(&format!("-{x}")));
    if has("KDE") {
        Some("konsole")
    } else if has("XFCE") {
        Some("xfce4-terminal")
    } else if has("MATE") {
        Some("mate-terminal")
    } else if has("GNOME") || has("UNITY") || has("CINNAMON") || has("BUDGIE") {
        Some("gnome-terminal")
    } else {
        None
    }
}

/// Lo que la detección necesita saber del entorno.
pub struct Probe<'a> {
    /// `general.terminal`.
    pub configured: &'a [String],
    /// claunch corre dentro de tmux (`$TMUX`).
    pub in_tmux: bool,
    /// Hay escritorio gráfico (`$DISPLAY` o `$WAYLAND_DISPLAY`).
    pub graphical: bool,
    /// `$TERMINAL`.
    pub terminal_env: Option<&'a str>,
    /// `$XDG_CURRENT_DESKTOP`.
    pub desktop: Option<&'a str>,
    /// El programa existe (en el PATH o como ruta).
    pub installed: &'a dyn Fn(&str) -> bool,
}

/// Elige dónde abrir sesiones.
pub fn choose(p: &Probe) -> Terminal {
    if !p.configured.is_empty() {
        if p.configured.len() == 1 && p.configured[0] == "tmux" {
            return Terminal::Tmux;
        }
        return Terminal::Emulator(p.configured.to_vec());
    }
    if p.in_tmux && (p.installed)("tmux") {
        return Terminal::Tmux;
    }
    if !p.graphical {
        return Terminal::Unavailable;
    }
    if let Some(t) = p.terminal_env {
        let mut words = t.split_whitespace();
        if let Some(prog) = words.next().filter(|prog| (p.installed)(prog)) {
            let mut cmd = vec![prog.to_string()];
            let extra: Vec<String> = words.map(String::from).collect();
            // Con argumentos propios (`kitty -1`) se respetan; los de ejecución van detrás.
            cmd.extend(extra);
            cmd.extend(exec_args(prog));
            return Terminal::Emulator(cmd);
        }
    }
    let preferred = p.desktop.and_then(desktop_terminal);
    let candidates = preferred.into_iter().chain(KNOWN.iter().map(|(n, _)| *n));
    for name in candidates {
        if (p.installed)(name) {
            let mut cmd = vec![name.to_string()];
            cmd.extend(exec_args(name));
            return Terminal::Emulator(cmd);
        }
    }
    Terminal::Unavailable
}

/// Detecta la terminal con el entorno real.
#[cfg(not(windows))]
pub fn detect(configured: &[String]) -> Terminal {
    let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let terminal_env = var("TERMINAL");
    let desktop = var("XDG_CURRENT_DESKTOP");
    let installed = |prog: &str| {
        if prog.contains('/') { std::path::Path::new(prog).is_file() } else { crate::paths::which(prog).is_some() }
    };
    choose(&Probe {
        configured,
        in_tmux: var("TMUX").is_some(),
        graphical: var("DISPLAY").is_some() || var("WAYLAND_DISPLAY").is_some(),
        terminal_env: terminal_env.as_deref(),
        desktop: desktop.as_deref(),
        installed: &installed,
    })
}

/// Una palabra de shell POSIX entre comillas simples.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Comando de shell con cada argumento entre comillas.
pub fn sh_join(cmd: &[String]) -> String {
    cmd.iter().map(|a| sh_quote(a)).collect::<Vec<_>>().join(" ")
}

/// tmux separa comandos con un argumento que termina en `;`: se escapa.
fn tmux_arg(s: &str) -> String {
    match s.strip_suffix(';') {
        Some(rest) => format!("{rest}\\;"),
        None => s.to_string(),
    }
}

/// Argumentos de cada invocación de `tmux`. Las ventanas de un set se abren en
/// segundo plano salvo la primera, que queda delante.
pub fn tmux_invocations(items: &[(&LaunchItem, Vec<String>)], placement: Placement) -> Vec<Vec<String>> {
    let cwd = |it: &LaunchItem| tmux_arg(&it.cwd.to_string_lossy());
    let new_window = |it: &LaunchItem, cmd: &[String], background: bool| {
        let mut a = vec!["new-window".to_string()];
        if background {
            a.push("-d".into());
        }
        a.extend(["-c".to_string(), cwd(it), "-n".into(), tmux_arg(&it.title), tmux_arg(&sh_join(cmd))]);
        a
    };
    match placement {
        Placement::Set(SetLayout::Panes, _) if items.len() > 1 => {
            let sep = || ";".to_string();
            let mut a = new_window(items[0].0, &items[0].1, false);
            for (it, cmd) in &items[1..] {
                a.extend([sep(), "split-window".into(), "-c".into(), cwd(it), tmux_arg(&sh_join(cmd))]);
                // Reparte tras cada división: sin espacio, la siguiente fallaría.
                a.extend([sep(), "select-layout".into(), "tiled".into()]);
            }
            a.extend([sep(), "select-pane".into(), "-t".into(), "{top-left}".into()]);
            vec![a]
        }
        Placement::Set(..) => items.iter().enumerate().map(|(i, (it, cmd))| new_window(it, cmd, i > 0)).collect(),
        Placement::NewTab | Placement::NewWindow => items.iter().map(|(it, cmd)| new_window(it, cmd, false)).collect(),
    }
}

/// Ejecuta las invocaciones de tmux y devuelve su error, si lo hay.
#[cfg(not(windows))]
pub fn run_tmux(items: &[(&LaunchItem, Vec<String>)], placement: Placement) -> anyhow::Result<()> {
    use anyhow::Context;
    for args in tmux_invocations(items, placement) {
        crate::info_log!("tmux {}", args.join(" "));
        let out = std::process::Command::new("tmux")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .context("no se pudo ejecutar tmux")?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            anyhow::bail!("tmux falló: {}", if err.is_empty() { format!("{}", out.status) } else { err });
        }
    }
    Ok(())
}

/// Abre una ventana del emulador por sesión. Sin tmux no hay pestañas ni
/// paneles comunes a todos los emuladores: un set en pestañas o paneles se abre
/// en ventanas separadas (queda en el log).
#[cfg(not(windows))]
pub fn spawn_emulator(prefix: &[String], items: &[(&LaunchItem, Vec<String>)], placement: Placement) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    use anyhow::Context;
    if matches!(placement, Placement::Set(SetLayout::Tabs | SetLayout::Panes, _)) && items.len() > 1 {
        crate::info_log!("sin tmux: el set se abre en {} ventanas separadas", items.len());
    }
    for (item, cmd) in items {
        let argv: Vec<String> = prefix.iter().chain(cmd.iter()).cloned().collect();
        crate::info_log!("{}", argv.join(" "));
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&item.cwd)
            // Su salida no debe ensuciar la TUI y cerrar esta terminal no debe cerrarlo.
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .with_context(|| format!("no se pudo ejecutar {} (configura general.terminal)", argv[0]))?;
        // Recoge el proceso al terminar para no dejar zombis.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::{SetWindow, Shell};

    fn probe<'a>(installed: &'a dyn Fn(&str) -> bool) -> Probe<'a> {
        Probe { configured: &[], in_tmux: false, graphical: true, terminal_env: None, desktop: None, installed }
    }

    fn emulator(t: Terminal) -> Vec<String> {
        match t {
            Terminal::Emulator(v) => v,
            _ => panic!("no es un emulador"),
        }
    }

    #[test]
    fn detection_order() {
        let all = |_: &str| true;
        let only = |n: &'static str| move |p: &str| p == n;

        // Configurado manda.
        let cfg = vec!["kitty".to_string(), "--single-instance".into()];
        assert_eq!(emulator(choose(&Probe { configured: &cfg, ..probe(&all) })), cfg);
        let tmux = vec!["tmux".to_string()];
        assert_eq!(choose(&Probe { configured: &tmux, ..probe(&all) }), Terminal::Tmux);

        // Dentro de tmux, tmux.
        assert_eq!(choose(&Probe { in_tmux: true, ..probe(&all) }), Terminal::Tmux);

        // Sin escritorio ni tmux, nada.
        assert_eq!(choose(&Probe { graphical: false, ..probe(&all) }), Terminal::Unavailable);

        // $TERMINAL con argumentos propios.
        let t = choose(&Probe { terminal_env: Some("alacritty --class claunch"), ..probe(&all) });
        assert_eq!(emulator(t), ["alacritty", "--class", "claunch", "-e"]);
        // $TERMINAL desconocido: `-e`.
        assert_eq!(emulator(choose(&Probe { terminal_env: Some("/opt/mi-term"), ..probe(&all) })), ["/opt/mi-term", "-e"]);

        // El del escritorio antes que la lista genérica.
        let t = choose(&Probe { desktop: Some("KDE"), ..probe(&all) });
        assert_eq!(emulator(t), ["konsole", "-e"]);
        let t = choose(&Probe { desktop: Some("ubuntu:GNOME"), ..probe(&all) });
        assert_eq!(emulator(t), ["gnome-terminal", "--"]);
        let t = choose(&Probe { desktop: Some("X-Cinnamon"), ..probe(&all) });
        assert_eq!(emulator(t), ["gnome-terminal", "--"]);

        // Lista genérica: el primero instalado.
        let kitty = only("kitty");
        assert_eq!(emulator(choose(&probe(&kitty))), ["kitty"]);
        let wez = only("wezterm");
        assert_eq!(emulator(choose(&Probe { desktop: Some("KDE"), ..probe(&wez) })), ["wezterm", "start", "--"]);
        let none = |_: &str| false;
        assert_eq!(choose(&probe(&none)), Terminal::Unavailable);
    }

    fn item(title: &str, cwd: &str) -> LaunchItem {
        LaunchItem {
            cwd: PathBuf::from(cwd),
            claude_args: vec![],
            title: title.into(),
            label: "app".into(),
            shell: Shell::GitBash,
            tab_color: None,
            fixed_title: false,
            session_id: None,
        }
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        assert_eq!(sh_join(&["/bin/sh".into(), "/tmp/a b.sh".into()]), "'/bin/sh' '/tmp/a b.sh'");
        assert_eq!(tmux_arg("App; prod;"), r"App; prod\;");
    }

    #[test]
    fn tmux_tabs_and_single() {
        let a = item("App", "/home/ana/app");
        let b = item("API;", "/home/ana/api");
        let cmd = vec!["/bin/sh".to_string(), "/tmp/x.sh".into()];
        let items = vec![(&a, cmd.clone()), (&b, cmd.clone())];

        let inv = tmux_invocations(&items[..1], Placement::NewTab);
        assert_eq!(inv, vec![vec!["new-window", "-c", "/home/ana/app", "-n", "App", "'/bin/sh' '/tmp/x.sh'"]]);

        let inv = tmux_invocations(&items, Placement::Set(SetLayout::Tabs, SetWindow::New));
        assert_eq!(inv.len(), 2);
        assert!(!inv[0].contains(&"-d".to_string()));
        assert_eq!(&inv[1][..2], ["new-window", "-d"]);
        assert!(inv[1].contains(&r"API\;".to_string()));
    }

    #[test]
    fn tmux_panes() {
        let its: Vec<LaunchItem> = (0..3).map(|i| item(&format!("p{i}"), &format!("/r/{i}"))).collect();
        let items: Vec<(&LaunchItem, Vec<String>)> = its.iter().map(|i| (i, vec!["/bin/sh".to_string()])).collect();
        let inv = tmux_invocations(&items, Placement::Set(SetLayout::Panes, SetWindow::Current));
        assert_eq!(inv.len(), 1);
        let s = inv[0].join(" ");
        assert!(s.starts_with("new-window -c /r/0 -n p0 '/bin/sh'"), "{s}");
        assert_eq!(s.matches("split-window").count(), 2);
        assert!(s.contains("split-window -c /r/2 '/bin/sh' ; select-layout tiled"));
        assert!(s.ends_with("; select-pane -t {top-left}"));
        // Un panel solo es una ventana normal.
        let one = tmux_invocations(&items[..1], Placement::Set(SetLayout::Panes, SetWindow::New));
        assert_eq!(one[0][0], "new-window");
        assert!(!one[0].contains(&"split-window".to_string()));
    }
}
