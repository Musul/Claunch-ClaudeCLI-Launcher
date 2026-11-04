//! Arranque de sesiones en Windows Terminal (pestañas, paneles o ventanas).
//!
//! Cadena por pestaña: `wt.exe` → shell (Git Bash por defecto) → `claunch run
//! <spec>` → `claude`. El paso intermedio fija el `cwd` con la unidad en
//! minúscula (MSYS siempre la pasa en mayúscula y `/resume` dejaría de ver las
//! sesiones de VS Code) y recibe los argumentos por un JSON UTF-8, sin las
//! capas de escape de wt, bash y cmd. Al arrancar deja una marca
//! `<token>.started` que la TUI usa para confirmar el arranque.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::config::{General, LaunchOptions, SetLayout, SetWindow, Shell};
use crate::paths;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    New,
    Resume(String),
    Fork(String),
}

#[derive(Debug, Clone)]
pub struct LaunchItem {
    pub cwd: PathBuf,
    pub claude_args: Vec<String>,
    pub title: String,
    /// Texto corto para el banner y los avisos (nombre del repo).
    pub label: String,
    pub shell: Shell,
    pub tab_color: Option<String>,
    /// Mantener el título fijo aunque Claude lo cambie.
    pub fixed_title: bool,
    pub session_id: Option<String>,
}

/// Cómo se reparten los elementos en Windows Terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    NewTab,
    NewWindow,
    Set(SetLayout, SetWindow),
}

/// Argumentos de `claude` para un modo y opciones. Devuelve también el id de
/// sesión si se conoce de antemano (las nuevas reciben `--session-id`).
pub fn claude_args(mode: &Mode, opts: &LaunchOptions) -> (Vec<String>, Option<String>) {
    let mut a = Vec::new();
    let id = match mode {
        Mode::New => {
            let id = uuid::Uuid::new_v4().to_string();
            a.extend(["--session-id".to_string(), id.clone()]);
            if let Some(n) = opts.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                a.extend(["-n".to_string(), n.to_string()]);
            }
            if let Some(w) = opts.worktree.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
                a.extend(["-w".to_string(), w.to_string()]);
            }
            Some(id)
        }
        Mode::Resume(id) => {
            a.extend(["-r".to_string(), id.clone()]);
            Some(id.clone())
        }
        Mode::Fork(id) => {
            a.extend(["-r".to_string(), id.clone(), "--fork-session".to_string()]);
            None
        }
    };
    if let Some(m) = opts.model.as_deref().filter(|m| !m.is_empty()) {
        a.extend(["--model".to_string(), m.to_string()]);
    }
    if let Some(p) = opts.permission_mode.as_deref().filter(|p| !p.is_empty()) {
        a.extend(["--permission-mode".to_string(), p.to_string()]);
    }
    (a, id)
}

/// Lo que `claunch run` necesita para arrancar Claude.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSpec {
    pub token: String,
    pub cwd: PathBuf,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub title: String,
    pub label: String,
    pub session_id: Option<String>,
}

/// Ejecutables resueltos una vez por arranque.
#[derive(Debug, Clone)]
pub struct Env {
    pub claude: Vec<String>,
    pub git_bash: Option<PathBuf>,
    pub wt: Option<PathBuf>,
    pub self_exe: PathBuf,
    pub general: General,
}

impl Env {
    pub fn resolve(general: &General) -> anyhow::Result<Env> {
        let claude = if !general.claude_command.is_empty() {
            general.claude_command.clone()
        } else if let Ok(json) = std::env::var("CLAUNCH_CLAUDE_COMMAND") {
            serde_json::from_str(&json).context("CLAUNCH_CLAUDE_COMMAND debe ser un array JSON")?
        } else {
            let p = paths::which("claude").context("no encuentro `claude` en el PATH")?;
            vec![p.to_string_lossy().into_owned()]
        };
        Ok(Env {
            claude,
            git_bash: paths::find_git_bash(Some(&general.git_bash)),
            wt: paths::find_wt(),
            self_exe: std::env::current_exe().context("no se pudo ubicar claunch.exe")?,
            general: general.clone(),
        })
    }

    fn spec_for(&self, item: &LaunchItem) -> RunSpec {
        RunSpec {
            token: uuid::Uuid::new_v4().simple().to_string(),
            cwd: paths::normalize(&item.cwd),
            program: PathBuf::from(&self.claude[0]),
            args: self.claude[1..].iter().cloned().chain(item.claude_args.iter().cloned()).collect(),
            title: item.title.clone(),
            label: item.label.clone(),
            session_id: item.session_id.clone(),
        }
    }

    /// Línea de comandos de la shell que ejecuta `claunch run <spec>`.
    fn shell_cmdline(&self, item: &LaunchItem, spec_path: &Path) -> anyhow::Result<Vec<String>> {
        let exe = self.self_exe.to_string_lossy().into_owned();
        let spec = spec_path.to_string_lossy().into_owned();
        let keep = self.general.keep_shell_open;
        Ok(match item.shell {
            Shell::GitBash => {
                let bash = self.git_bash.as_ref().context("no encuentro Git Bash (configura general.git_bash)")?;
                let script = spec_path.with_extension("sh");
                let q = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
                let mut body = String::from("# claunch: arranque generado, se borra solo.\nexport CHERE_INVOKING=1\n");
                body += &format!("cd {} 2>/dev/null\n", q(&paths::to_msys(&item.cwd)));
                body += &format!(
                    "MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' {} run {}\n",
                    q(&paths::to_msys(&self.self_exe)),
                    q(&spec.replace('\\', "/"))
                );
                body += "rm -f -- \"$0\"\n";
                if keep {
                    body += "exec bash --login -i\n";
                }
                crate::util::write_atomic(&script, body.as_bytes())?;
                vec![bash.to_string_lossy().into_owned(), "--login".into(), script.to_string_lossy().replace('\\', "/")]
            }
            Shell::Powershell => {
                let sq = |s: &str| format!("'{}'", s.replace('\'', "''"));
                let mut v = vec!["powershell.exe".to_string(), "-NoLogo".into()];
                if keep {
                    v.push("-NoExit".into());
                }
                v.extend(["-Command".to_string(), format!("& {} run {}", sq(&exe), sq(&spec))]);
                v
            }
            Shell::Cmd => {
                vec!["cmd.exe".into(), if keep { "/k" } else { "/c" }.into(), "call".into(), exe, "run".into(), spec]
            }
        })
    }

    /// Prepara specs y lanza. Devuelve los tokens a confirmar.
    pub fn launch(&self, items: &[LaunchItem], placement: Placement) -> anyhow::Result<Vec<(String, String)>> {
        if items.is_empty() {
            bail!("no hay sesiones que abrir");
        }
        let dir = paths::launch_dir();
        std::fs::create_dir_all(&dir)?;
        let mut prepared = Vec::with_capacity(items.len());
        let mut tokens = Vec::with_capacity(items.len());
        for item in items {
            if !item.cwd.is_dir() {
                bail!("la carpeta {} no existe", item.cwd.display());
            }
            let spec = self.spec_for(item);
            let spec_path = dir.join(format!("{}.json", spec.token));
            crate::util::write_atomic(&spec_path, &serde_json::to_vec_pretty(&spec)?)?;
            let cmd = self.shell_cmdline(item, &spec_path)?;
            tokens.push((spec.token.clone(), item.title.clone()));
            prepared.push((item, cmd));
        }

        let Some(wt) = &self.wt else {
            // Sin Windows Terminal: una consola nueva por sesión.
            for (item, cmd) in &prepared {
                spawn_console(cmd, &item.cwd)?;
            }
            return Ok(tokens);
        };
        let profile = (!self.general.wt_profile.is_empty()).then_some(self.general.wt_profile.as_str());
        for args in wt_invocations(&prepared, placement, profile) {
            crate::info_log!("wt {}", args.join(" "));
            Command::new(wt).args(&args).spawn().with_context(|| format!("no se pudo ejecutar {}", wt.display()))?;
        }
        Ok(tokens)
    }

    /// Arranca en la terminal actual y espera (la TUI se suspende antes).
    pub fn run_here(&self, item: &LaunchItem) -> anyhow::Result<i32> {
        let spec = self.spec_for(item);
        run_spec(&spec, false)
    }
}

fn spawn_console(cmd: &[String], cwd: &Path) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x10;
        Command::new(&cmd[0]).args(&cmd[1..]).current_dir(cwd).creation_flags(CREATE_NEW_CONSOLE).spawn()?;
    }
    #[cfg(not(windows))]
    {
        Command::new(&cmd[0]).args(&cmd[1..]).current_dir(cwd).spawn()?;
    }
    Ok(())
}

/// wt separa subcomandos con `;`: dentro de un argumento hay que escaparlo.
fn wt_escape(s: &str) -> String {
    s.replace(';', r"\;")
}

fn pane_args(item: &LaunchItem, cmd: &[String], profile: Option<&str>) -> Vec<String> {
    let mut a = Vec::new();
    if let Some(p) = profile {
        a.extend(["-p".to_string(), wt_escape(p)]);
    }
    a.extend(["--title".to_string(), wt_escape(&item.title)]);
    if item.fixed_title {
        a.push("--suppressApplicationTitle".into());
    }
    if let Some(c) = item.tab_color.as_deref().filter(|c| !c.is_empty()) {
        a.extend(["--tabColor".to_string(), wt_escape(c)]);
    }
    a.extend(["-d".to_string(), wt_escape(&item.cwd.to_string_lossy())]);
    a.extend(cmd.iter().map(|c| wt_escape(c)));
    a
}

/// Columnas y filas por columna para `n` paneles (columnas de igual ancho).
pub fn pane_grid(n: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let cols = (n as f64).sqrt().ceil() as usize;
    let base = n / cols;
    let extra = n % cols;
    (0..cols).map(|c| if c >= cols - extra { base + 1 } else { base }).collect()
}

fn frac(num: usize, den: usize) -> String {
    format!("{:.3}", num as f64 / den as f64)
}

/// Argumentos de cada invocación de `wt.exe`.
pub fn wt_invocations(items: &[(&LaunchItem, Vec<String>)], placement: Placement, profile: Option<&str>) -> Vec<Vec<String>> {
    let sep = || ";".to_string();
    let window = |w: SetWindow| match w {
        SetWindow::New => "new",
        SetWindow::Current => "0",
    };
    match placement {
        Placement::NewTab | Placement::NewWindow => {
            let w = if placement == Placement::NewTab { "0" } else { "new" };
            items
                .iter()
                .map(|(it, cmd)| {
                    let mut a = vec!["-w".to_string(), w.into(), "new-tab".into()];
                    a.extend(pane_args(it, cmd, profile));
                    a
                })
                .collect()
        }
        Placement::Set(SetLayout::Windows, _) => items
            .iter()
            .map(|(it, cmd)| {
                let mut a = vec!["-w".to_string(), "new".into(), "new-tab".into()];
                a.extend(pane_args(it, cmd, profile));
                a
            })
            .collect(),
        Placement::Set(SetLayout::Tabs, w) => {
            let mut a = vec!["-w".to_string(), window(w).into()];
            for (i, (it, cmd)) in items.iter().enumerate() {
                if i > 0 {
                    a.push(sep());
                }
                a.push("new-tab".into());
                a.extend(pane_args(it, cmd, profile));
            }
            if items.len() > 1 && w == SetWindow::New {
                a.extend([sep(), "focus-tab".into(), "-t".into(), "0".into()]);
            }
            vec![a]
        }
        Placement::Set(SetLayout::Panes, w) => {
            let grid = pane_grid(items.len());
            let cols = grid.len();
            // Orden por columnas: los primeros `grid[0]` van en la columna 0, etc.
            let mut starts = Vec::with_capacity(cols);
            let mut acc = 0;
            for rows in &grid {
                starts.push(acc);
                acc += rows;
            }
            let mut a = vec!["-w".to_string(), window(w).into(), "new-tab".into()];
            a.extend(pane_args(items[0].0, &items[0].1, profile));
            for c in 1..cols {
                let (it, cmd) = &items[starts[c]];
                a.extend([sep(), "split-pane".into(), "-V".into(), "-s".into(), frac(cols - c, cols - c + 1)]);
                a.extend(pane_args(it, cmd, profile));
            }
            for c in 0..cols {
                let rows = grid[c];
                if rows < 2 {
                    continue;
                }
                a.extend([sep(), "focus-pane".into(), "-t".into(), c.to_string()]);
                for r in 1..rows {
                    let (it, cmd) = &items[starts[c] + r];
                    a.extend([sep(), "split-pane".into(), "-H".into(), "-s".into(), frac(rows - r, rows - r + 1)]);
                    a.extend(pane_args(it, cmd, profile));
                }
            }
            if items.len() > 1 {
                a.extend([sep(), "focus-pane".into(), "-t".into(), "0".into()]);
            }
            vec![a]
        }
    }
}

// ---------------------------------------------------------------- claunch run

/// Ignora Ctrl+C en claunch mientras Claude corre (Claude lo gestiona).
/// Un handler propio, a diferencia de `SetConsoleCtrlHandler(NULL, TRUE)`, no
/// lo heredan los procesos hijos.
#[cfg(windows)]
fn ignore_ctrl_c() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe extern "system" fn handler(_ctrl: u32) -> windows_sys::core::BOOL {
        1
    }
    // SAFETY: registra una función estática sin estado.
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

#[cfg(not(windows))]
fn ignore_ctrl_c() {}

const ACCENT: &str = "\x1b[38;2;217;119;87m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

fn run_spec(spec: &RunSpec, set_title: bool) -> anyhow::Result<i32> {
    if set_title {
        print!("\x1b]0;{}\x07", spec.title.replace(['\x07', '\x1b'], ""));
    }
    println!("{ACCENT}✳{RESET} {DIM}claunch ·{RESET} {BOLD}{}{RESET} {DIM}· {}{RESET}", spec.label, spec.title);
    ignore_ctrl_c();
    let status = Command::new(&spec.program)
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .status()
        .with_context(|| format!("no se pudo ejecutar {}", spec.program.display()))?;
    let code = status.code().unwrap_or(1);
    match &spec.session_id {
        Some(id) => println!("\n{DIM}↩ Para retomar: claude -r {id}   ·   claunch para elegir otra{RESET}"),
        None => println!("\n{DIM}↩ claunch para elegir otra sesión{RESET}"),
    }
    Ok(code)
}

/// `claunch run <spec.json>`: lo invoca la shell de cada pestaña.
pub fn run_main(args: &[String]) -> ExitCode {
    let Some(spec_path) = args.first().map(PathBuf::from) else {
        eprintln!("uso: claunch run <spec.json>");
        return ExitCode::from(2);
    };
    let spec: RunSpec = match std::fs::read(&spec_path).map_err(anyhow::Error::from).and_then(|b| Ok(serde_json::from_slice(&b)?)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("claunch: no se pudo leer {}: {e}", spec_path.display());
            crate::warn_log!("run: spec ilegible {}: {e}", spec_path.display());
            return ExitCode::from(2);
        }
    };
    if let Err(e) = std::fs::remove_file(&spec_path) {
        crate::warn_log!("run: no se pudo borrar {}: {e}", spec_path.display());
    }
    let marker = spec_path.with_file_name(format!("{}.started", spec.token));
    if let Err(e) = std::fs::write(&marker, std::process::id().to_string()) {
        crate::warn_log!("run: no se pudo escribir la marca {}: {e}", marker.display());
    }
    match run_spec(&spec, true) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("{ACCENT}claunch:{RESET} {e:#}");
            crate::warn_log!("run: {e:#}");
            ExitCode::from(1)
        }
    }
}

/// Borra restos de arranques viejos (más de un día).
pub fn cleanup_launch_dir() {
    let Ok(entries) = std::fs::read_dir(paths::launch_dir()) else { return };
    let day = std::time::Duration::from_secs(86_400);
    for e in entries.flatten() {
        let old = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_some_and(|age| age > day);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(title: &str) -> LaunchItem {
        LaunchItem {
            cwd: PathBuf::from(r"c:\r\app"),
            claude_args: vec![],
            title: title.into(),
            label: "app".into(),
            shell: Shell::GitBash,
            tab_color: Some("#D97757".into()),
            fixed_title: false,
            session_id: None,
        }
    }

    #[test]
    fn grid_shapes() {
        assert_eq!(pane_grid(1), vec![1]);
        assert_eq!(pane_grid(2), vec![1, 1]);
        assert_eq!(pane_grid(3), vec![1, 2]);
        assert_eq!(pane_grid(4), vec![2, 2]);
        assert_eq!(pane_grid(5), vec![1, 2, 2]);
        assert_eq!(pane_grid(7), vec![2, 2, 3]);
        assert_eq!(pane_grid(7).iter().sum::<usize>(), 7);
    }

    #[test]
    fn args_for_modes() {
        let opts = LaunchOptions { model: Some("opus".into()), name: Some("x".into()), ..Default::default() };
        let (a, id) = claude_args(&Mode::New, &opts);
        assert_eq!(a[0], "--session-id");
        assert_eq!(id.as_deref(), Some(a[1].as_str()));
        assert!(a.windows(2).any(|w| w == ["-n", "x"]));
        let (a, id) = claude_args(&Mode::Fork("abc".into()), &LaunchOptions::default());
        assert_eq!(a, vec!["-r", "abc", "--fork-session"]);
        assert_eq!(id, None);
    }

    #[test]
    fn tabs_set_and_escaping() {
        let a = item("App; prod");
        let b = item("VM");
        let cmd = vec!["bash.exe".to_string(), "--login".into(), "x.sh".into()];
        let items = vec![(&a, cmd.clone()), (&b, cmd)];
        let inv = wt_invocations(&items, Placement::Set(SetLayout::Tabs, SetWindow::New), None);
        assert_eq!(inv.len(), 1);
        let v = &inv[0];
        assert_eq!(&v[..3], ["-w", "new", "new-tab"]);
        assert!(v.contains(&r"App\; prod".to_string()));
        assert_eq!(v.iter().filter(|s| *s == ";").count(), 2); // 2.ª pestaña + focus-tab
    }

    #[test]
    fn panes_set_layout() {
        let its: Vec<LaunchItem> = (0..3).map(|i| item(&format!("p{i}"))).collect();
        let items: Vec<(&LaunchItem, Vec<String>)> = its.iter().map(|i| (i, vec!["cmd.exe".to_string()])).collect();
        let inv = wt_invocations(&items, Placement::Set(SetLayout::Panes, SetWindow::Current), None);
        let s = inv[0].join(" ");
        assert!(s.starts_with("-w 0 new-tab"));
        assert!(s.contains("split-pane -V -s 0.500"));
        assert!(s.contains("focus-pane -t 1 ; split-pane -H -s 0.500"));
        let windows = wt_invocations(&items, Placement::Set(SetLayout::Windows, SetWindow::New), None);
        assert_eq!(windows.len(), 3);
    }
}
