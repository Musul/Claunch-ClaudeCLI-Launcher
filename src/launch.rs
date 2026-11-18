//! Arranque de sesiones en pestañas, paneles o ventanas.
//!
//! Windows. Cadena por pestaña: `wt.exe` → shell (Git Bash por defecto) →
//! `claunch run <spec>` → el harness (`claude`, `codex`, `opencode`, `agy`,
//! `pi`). El paso intermedio fija el `cwd` con la grafía que espera el harness
//! (Claude, con la unidad en minúscula: MSYS siempre la pasa en mayúscula y
//! `/resume` dejaría de ver las sesiones de VS Code) y recibe los argumentos
//! por un JSON UTF-8, sin las capas de escape de wt, bash y cmd.
//!
//! Linux. tmux o un emulador gráfico (ver `terminal`) → `/bin/sh <script>` →
//! `claunch run <spec>` → el harness.
//!
//! Al arrancar, `claunch run` deja una marca `<token>.started` que la TUI usa
//! para confirmar el arranque y, para los harness que no publican sus procesos,
//! un aviso de sesión abierta (ver `data::live`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::config::{Config, General, SetLayout, SetWindow, Shell};
use crate::harness::Harness;
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
    pub harness: Harness,
    /// Argumentos del harness, sin el ejecutable (ver `Harness::args`).
    pub args: Vec<String>,
    pub title: String,
    /// Texto corto para el banner y los avisos (nombre del repo).
    pub label: String,
    /// Solo Windows: en Linux la sesión arranca con `/bin/sh`.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub shell: Shell,
    pub tab_color: Option<String>,
    /// Mantener el título fijo aunque el harness lo cambie.
    pub fixed_title: bool,
    /// Id de la sesión, si se conoce antes de arrancar.
    pub session_id: Option<String>,
}

/// Cómo se reparten los elementos (pestañas de Windows Terminal o ventanas de tmux).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    NewTab,
    NewWindow,
    Set(SetLayout, SetWindow),
}

/// Lo que `claunch run` necesita para arrancar el harness.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSpec {
    pub token: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub harness: Harness,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub title: String,
    pub label: String,
    pub session_id: Option<String>,
}

/// Dónde se abren las pestañas y ventanas nuevas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    /// Windows Terminal (`wt.exe`).
    #[cfg_attr(not(windows), allow(dead_code))]
    Wt(PathBuf),
    /// Windows sin Windows Terminal: una consola nueva por sesión.
    #[cfg_attr(not(windows), allow(dead_code))]
    Console,
    /// Linux dentro de tmux: ventanas y paneles de la sesión de tmux actual.
    Tmux,
    /// Linux con escritorio: emulador y argumentos previos al comando; una
    /// ventana por sesión.
    Emulator(Vec<String>),
    /// Linux sin tmux ni escritorio (SSH): solo «Esta terminal».
    Unavailable,
}

/// Ejecutables resueltos una vez por arranque.
#[derive(Debug, Clone)]
pub struct Env {
    /// Comando (ejecutable y argumentos previos) de cada harness disponible:
    /// activo en la config y con ejecutable localizado.
    pub commands: BTreeMap<Harness, Vec<String>>,
    /// Harness cuyo comando está mal configurado (config o variable de entorno
    /// ilegible), con el motivo. Los demás siguen funcionando.
    pub problems: BTreeMap<Harness, String>,
    /// Git Bash (Windows).
    #[cfg_attr(not(windows), allow(dead_code))]
    pub git_bash: Option<PathBuf>,
    pub terminal: Terminal,
    pub self_exe: PathBuf,
    pub general: General,
}

/// Comando de un harness: el de la config, el de `CLAUNCH_<ID>_COMMAND` (un
/// array JSON, para pruebas) o el ejecutable que encuentre `Harness::locate`.
/// `None` si no está instalado.
fn harness_command(cfg: &Config, h: Harness) -> anyhow::Result<Option<Vec<String>>> {
    let configured = cfg.harness_command(h);
    let var = format!("CLAUNCH_{}_COMMAND", h.id().to_ascii_uppercase());
    let cmd = if !configured.is_empty() {
        configured
    } else if let Ok(json) = std::env::var(&var) {
        let cmd: Vec<String> = serde_json::from_str(&json).with_context(|| format!("{var} debe ser un array JSON"))?;
        anyhow::ensure!(!cmd.is_empty(), "{var} no puede ser un array vacío");
        cmd
    } else {
        return Ok(h.locate().map(|p| vec![p.to_string_lossy().into_owned()]));
    };
    Ok(Some(resolve_program(cmd, paths::which)))
}

/// Un ejecutable escrito sin ruta (`codex`, `npx`) se busca en el PATH: en
/// Windows los que instala npm son `.cmd`, y `Command` solo encuentra por
/// nombre los `.exe`. Si no aparece, se deja como está.
fn resolve_program(mut cmd: Vec<String>, which: impl Fn(&str) -> Option<PathBuf>) -> Vec<String> {
    if let Some(program) = cmd.first_mut()
        && !program.contains(['/', '\\'])
        && let Some(found) = which(program)
    {
        *program = found.to_string_lossy().into_owned();
    }
    cmd
}

impl Env {
    pub fn resolve(cfg: &Config) -> anyhow::Result<Env> {
        let general = &cfg.general;
        let mut commands = BTreeMap::new();
        let mut problems = BTreeMap::new();
        for h in Harness::ALL {
            if !cfg.harness_enabled(h) {
                continue;
            }
            // Un harness mal configurado no deja sin lanzar a los demás.
            match harness_command(cfg, h) {
                Ok(Some(cmd)) => {
                    commands.insert(h, cmd);
                }
                Ok(None) => {}
                Err(e) => {
                    problems.insert(h, format!("{e:#}"));
                }
            }
        }
        #[cfg(windows)]
        let (git_bash, terminal) =
            (paths::find_git_bash(Some(&general.git_bash)), paths::find_wt().map_or(Terminal::Console, Terminal::Wt));
        #[cfg(not(windows))]
        let (git_bash, terminal) = (None, crate::terminal::detect(&general.terminal));
        Ok(Env {
            commands,
            problems,
            git_bash,
            terminal,
            self_exe: std::env::current_exe().context("no se pudo ubicar el ejecutable de claunch")?,
            general: general.clone(),
        })
    }

    /// Harness que se pueden lanzar, en su orden habitual.
    pub fn installed(&self) -> Vec<Harness> {
        self.commands.keys().copied().collect()
    }

    /// Comando del harness, o por qué no se puede lanzar.
    pub fn command(&self, h: Harness) -> anyhow::Result<&[String]> {
        let i = h.info();
        if let Some(problem) = self.problems.get(&h) {
            bail!("{} mal configurado: {problem}", i.label);
        }
        self.commands.get(&h).map(Vec::as_slice).with_context(|| {
            format!(
                "no encuentro `{}` ({}). Instálalo ({}) o indica su ruta en [harness.{}] command",
                i.program, i.label, i.install, i.id
            )
        })
    }

    fn spec_for(&self, item: &LaunchItem) -> anyhow::Result<RunSpec> {
        let cmd = self.command(item.harness)?;
        Ok(RunSpec {
            token: uuid::Uuid::new_v4().simple().to_string(),
            // Claude compara el `cwd` como texto: siempre con la unidad en minúscula.
            cwd: if item.harness == Harness::Claude { paths::normalize(&item.cwd) } else { item.cwd.clone() },
            harness: item.harness,
            program: PathBuf::from(&cmd[0]),
            args: cmd[1..].iter().cloned().chain(item.args.iter().cloned()).collect(),
            title: item.title.clone(),
            label: item.label.clone(),
            session_id: item.session_id.clone(),
        })
    }

    /// Línea de comandos de la shell que ejecuta `claunch run <spec>`.
    #[cfg(windows)]
    fn shell_cmdline(&self, item: &LaunchItem, spec_path: &Path) -> anyhow::Result<Vec<String>> {
        let exe = self.self_exe.to_string_lossy().into_owned();
        let spec = spec_path.to_string_lossy().into_owned();
        let keep = self.general.keep_shell_open;
        Ok(match item.shell {
            Shell::GitBash => {
                let bash = self.git_bash.as_ref().context("no encuentro Git Bash (configura general.git_bash)")?;
                let script = spec_path.with_extension("sh");
                let q = |s: &str| crate::terminal::sh_quote(s);
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

    /// Script de `/bin/sh` que ejecuta `claunch run <spec>` (la shell elegida
    /// en `item.shell` solo aplica a Windows). Si se deja abierta, sigue en `$SHELL`.
    #[cfg(not(windows))]
    fn shell_cmdline(&self, item: &LaunchItem, spec_path: &Path) -> anyhow::Result<Vec<String>> {
        use crate::terminal::sh_quote as q;
        let script = spec_path.with_extension("sh");
        let mut body = String::from("# claunch: arranque generado, se borra solo.\n");
        // Ctrl+C llega también a esta shell: sin trap, dash termina al salir
        // el harness y se pierden el aviso para retomar y la shell. El trap no
        // lo heredan los hijos (vuelve al valor por defecto en exec).
        body += "trap : INT\n";
        body += &format!("cd {} 2>/dev/null\n", q(&item.cwd.to_string_lossy()));
        body += &format!("{} run {}\n", q(&self.self_exe.to_string_lossy()), q(&spec_path.to_string_lossy()));
        body += "rm -f -- \"$0\"\n";
        if self.general.keep_shell_open {
            body += "exec \"${SHELL:-/bin/sh}\"\n";
        }
        crate::util::write_atomic(&script, body.as_bytes())?;
        Ok(vec!["/bin/sh".into(), script.to_string_lossy().into_owned()])
    }

    /// Prepara specs y lanza. Devuelve los tokens a confirmar.
    pub fn launch(&self, items: &[LaunchItem], placement: Placement) -> anyhow::Result<Vec<(String, String)>> {
        if items.is_empty() {
            bail!("no hay sesiones que abrir");
        }
        if self.terminal == Terminal::Unavailable {
            bail!(
                "no hay dónde abrir pestañas: claunch no corre dentro de tmux ni en un escritorio gráfico. \
                 Ábrela en «Esta terminal» (tecla o → Destino), entra en tmux o configura general.terminal"
            );
        }
        // Todo se comprueba antes de escribir nada: un set no se abre a medias.
        let mut specs = Vec::with_capacity(items.len());
        for item in items {
            if !item.cwd.is_dir() {
                bail!("la carpeta {} no existe", item.cwd.display());
            }
            specs.push(self.spec_for(item)?);
        }
        let dir = paths::launch_dir();
        std::fs::create_dir_all(&dir)?;
        let mut prepared = Vec::with_capacity(items.len());
        let mut tokens = Vec::with_capacity(items.len());
        for (item, spec) in items.iter().zip(specs) {
            let spec_path = dir.join(format!("{}.json", spec.token));
            crate::util::write_atomic(&spec_path, &serde_json::to_vec_pretty(&spec)?)?;
            let cmd = self.shell_cmdline(item, &spec_path)?;
            tokens.push((spec.token.clone(), item.title.clone()));
            prepared.push((item, cmd));
        }

        match &self.terminal {
            Terminal::Wt(wt) => {
                let profile = (!self.general.wt_profile.is_empty()).then_some(self.general.wt_profile.as_str());
                for args in wt_invocations(&prepared, placement, profile) {
                    crate::info_log!("wt {}", args.join(" "));
                    Command::new(wt).args(&args).spawn().with_context(|| format!("no se pudo ejecutar {}", wt.display()))?;
                }
            }
            Terminal::Console => {
                for (item, cmd) in &prepared {
                    spawn_console(cmd, &item.cwd)?;
                }
            }
            #[cfg(not(windows))]
            Terminal::Tmux => crate::terminal::run_tmux(&prepared, placement)?,
            #[cfg(not(windows))]
            Terminal::Emulator(prefix) => crate::terminal::spawn_emulator(prefix, &prepared, placement)?,
            other => bail!("{other:?} no está disponible en este sistema"),
        }
        Ok(tokens)
    }

    /// Arranca en la terminal actual y espera (la TUI se suspende antes).
    pub fn run_here(&self, item: &LaunchItem) -> anyhow::Result<i32> {
        if !item.cwd.is_dir() {
            bail!("la carpeta {} no existe", item.cwd.display());
        }
        let spec = self.spec_for(item)?;
        run_spec(&spec, false, &crate::data::live::markers_dir())
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
#[cfg_attr(not(windows), allow(dead_code))]
fn wt_escape(s: &str) -> String {
    s.replace(';', r"\;")
}

#[cfg_attr(not(windows), allow(dead_code))]
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

#[cfg_attr(not(windows), allow(dead_code))]
fn frac(num: usize, den: usize) -> String {
    format!("{:.3}", num as f64 / den as f64)
}

/// Argumentos de cada invocación de `wt.exe`.
#[cfg_attr(not(windows), allow(dead_code))]
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

/// Ignora Ctrl+C en claunch mientras el harness corre (él lo gestiona).
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

/// Ignora SIGINT en claunch mientras el harness corre: Ctrl+C también llega a
/// claunch (mismo grupo de procesos) y, si lo matara, la shell retomaría la
/// terminal con el harness aún vivo. Un handler propio, a diferencia de
/// `SIG_IGN`, vuelve al valor por defecto en `exec`: el harness no lo hereda.
#[cfg(unix)]
fn ignore_ctrl_c() {
    extern "C" fn handler(_sig: libc::c_int) {}
    // SAFETY: registra una función sin estado que no hace nada.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as extern "C" fn(libc::c_int) as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
}

#[cfg(not(any(windows, unix)))]
fn ignore_ctrl_c() {}

const ACCENT: &str = "\x1b[38;2;217;119;87m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// Arranca el harness y espera. `markers` es la carpeta donde se avisa de que
/// la sesión está abierta (la de la TUI que la lanzó).
fn run_spec(spec: &RunSpec, set_title: bool, markers: &Path) -> anyhow::Result<i32> {
    if set_title {
        print!("\x1b]0;{}\x07", spec.title.replace(['\x07', '\x1b'], ""));
    }
    let agent = if spec.harness == Harness::Claude { String::new() } else { format!(" {DIM}· {}{RESET}", spec.harness.label()) };
    println!("{ACCENT}✳{RESET} {DIM}claunch ·{RESET} {BOLD}{}{RESET} {DIM}· {}{RESET}{agent}", spec.label, spec.title);
    // Lo que este ejecutable no admite se quita antes de arrancar (la consulta
    // puede tardar unos segundos: Ctrl+C aún corta). El aviso se repite al
    // salir, por si el harness limpia la pantalla.
    let mut args = spec.args.clone();
    let note = spec.harness.preflight(&spec.program, &mut args).map(|n| format!("{ACCENT}claunch:{RESET} {n}"));
    if let Some(n) = &note {
        println!("{n}");
    }
    ignore_ctrl_c();
    // Hora anterior al proceso: ninguna sesión suya puede nacer antes.
    let started_ms = crate::util::now_ms();
    let mut child = Command::new(&spec.program)
        .args(&args)
        .current_dir(&spec.cwd)
        .spawn()
        .with_context(|| format!("no se pudo ejecutar {}", spec.program.display()))?;
    // Claude publica sus procesos; para el resto, claunch avisa de que la
    // sesión está abierta mientras dure el proceso.
    let marker = (spec.harness != Harness::Claude).then(|| crate::data::live::publish(markers, spec, child.id(), started_ms)).flatten();
    let status = child.wait();
    if let Some(m) = marker {
        let _ = std::fs::remove_file(m);
    }
    let code = status.with_context(|| format!("se perdió el proceso de {}", spec.program.display()))?.code().unwrap_or(1);
    if let Some(n) = &note {
        println!("\n{n}");
    }
    match &spec.session_id {
        Some(id) => println!("\n{DIM}↩ Para retomar: {}   ·   claunch para elegir otra{RESET}", spec.harness.resume_command(id)),
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
    // El aviso de sesión abierta va junto al spec, en la carpeta de datos de la
    // TUI que lanzó: esta pestaña puede no heredar su `CLAUNCH_DATA_DIR`.
    let markers = markers_dir_for(&spec_path);
    match run_spec(&spec, true, &markers) {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("{ACCENT}claunch:{RESET} {e:#}");
            crate::warn_log!("run: {e:#}");
            ExitCode::from(1)
        }
    }
}

/// Carpeta de avisos de la TUI que escribió el spec: `<datos>/launch/<token>.json`
/// → `<datos>/live`. Un spec suelto (de una prueba a mano, o con ruta
/// relativa) no dice de qué carpeta de datos es: la de este proceso.
fn markers_dir_for(spec_path: &Path) -> PathBuf {
    let launch = spec_path.parent().filter(|dir| dir.is_absolute() && dir.file_name().is_some_and(|n| n == "launch"));
    launch.and_then(Path::parent).map(|data| data.join("live")).unwrap_or_else(crate::data::live::markers_dir)
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
            harness: Harness::Claude,
            args: vec![],
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

    fn env(commands: &[(Harness, &[&str])]) -> Env {
        Env {
            commands: commands.iter().map(|(h, c)| (*h, c.iter().map(|s| s.to_string()).collect())).collect(),
            problems: BTreeMap::new(),
            git_bash: None,
            terminal: Terminal::Unavailable,
            self_exe: PathBuf::from("claunch"),
            general: General::default(),
        }
    }

    #[test]
    fn spec_uses_the_command_of_each_harness() {
        let e = env(&[(Harness::Claude, &["claude"]), (Harness::Codex, &["npx", "codex"])]);
        assert_eq!(e.installed(), [Harness::Claude, Harness::Codex]);

        let mut it = item("x");
        it.harness = Harness::Codex;
        it.cwd = PathBuf::from(r"C:\r\App");
        it.args = vec!["resume".into(), "abc".into()];
        let spec = e.spec_for(&it).unwrap();
        assert_eq!(spec.program, PathBuf::from("npx"));
        assert_eq!(spec.args, ["codex", "resume", "abc"]);
        // Solo Claude recibe el `cwd` normalizado; el resto, tal cual.
        assert_eq!(spec.cwd, PathBuf::from(r"C:\r\App"));
        assert_eq!(spec.harness, Harness::Codex);

        it.harness = Harness::Pi;
        let err = e.spec_for(&it).unwrap_err().to_string();
        assert!(err.contains("`pi`") && err.contains("[harness.pi]"), "{err}");
        // Sin dónde abrir, el fallo avisa antes de escribir nada.
        assert!(e.launch(&[it], Placement::NewTab).is_err());
    }

    #[test]
    fn bare_program_names_are_resolved_in_the_path() {
        // `codex` instalado con npm en Windows es `codex.cmd`: hay que dar la ruta completa.
        let which = |name: &str| (name == "codex").then(|| PathBuf::from("/npm/codex.cmd"));
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(resolve_program(s(&["codex", "-m", "x"]), which), s(&["/npm/codex.cmd", "-m", "x"]));
        // Con ruta o sin encontrar, se respeta lo escrito.
        assert_eq!(resolve_program(s(&["./bin/codex"]), which), s(&["./bin/codex"]));
        assert_eq!(resolve_program(s(&[r"C:\bin\codex.exe"]), which), s(&[r"C:\bin\codex.exe"]));
        assert_eq!(resolve_program(s(&["otro"]), which), s(&["otro"]));
        assert!(resolve_program(Vec::new(), which).is_empty());
    }

    #[test]
    fn markers_go_next_to_the_spec() {
        let data = std::env::temp_dir().join("claunch-tests").join("datos-tui");
        assert_eq!(markers_dir_for(&data.join("launch").join("abc.json")), data.join("live"));
        // Un spec suelto o con ruta relativa no dice de qué carpeta de datos
        // es: la de este proceso.
        let own = crate::data::live::markers_dir();
        assert_eq!(markers_dir_for(Path::new("abc.json")), own);
        assert_eq!(markers_dir_for(Path::new("launch/abc.json")), own);
        assert_eq!(markers_dir_for(&data.join("abc.json")), own);
        assert_eq!(markers_dir_for(&data.join("otra").join("abc.json")), own);
    }

    #[test]
    fn a_misconfigured_harness_does_not_block_the_rest() {
        let mut e = env(&[(Harness::Claude, &["claude"])]);
        e.problems.insert(Harness::Pi, "CLAUNCH_PI_COMMAND debe ser un array JSON".into());
        assert!(e.command(Harness::Claude).is_ok());
        let err = e.command(Harness::Pi).unwrap_err().to_string();
        assert!(err.contains("Pi mal configurado") && err.contains("array JSON"), "{err}");
        assert_eq!(e.installed(), [Harness::Claude]);
    }

    #[cfg(windows)]
    #[test]
    fn claude_cwd_keeps_lowercase_drive() {
        let e = env(&[(Harness::Claude, &["claude"])]);
        let mut it = item("x");
        it.cwd = PathBuf::from(r"C:\r\App");
        assert_eq!(e.spec_for(&it).unwrap().cwd, PathBuf::from(r"c:\r\App"));
    }

    #[test]
    fn run_spec_roundtrips_and_old_specs_default_to_claude() {
        let old = r#"{"token":"t","cwd":"/r","program":"claude","args":[],"title":"a","label":"b","session_id":null}"#;
        let spec: RunSpec = serde_json::from_str(old).unwrap();
        assert_eq!(spec.harness, Harness::Claude);
        let new = RunSpec { harness: Harness::Opencode, ..spec };
        let back: RunSpec = serde_json::from_str(&serde_json::to_string(&new).unwrap()).unwrap();
        assert_eq!(back.harness, Harness::Opencode);
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
