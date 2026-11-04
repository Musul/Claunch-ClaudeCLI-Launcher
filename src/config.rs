//! Configuración y sets de sesiones: `%APPDATA%\claunch\config.toml`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Shell {
    #[default]
    GitBash,
    Powershell,
    Cmd,
}

impl Shell {
    pub const ALL: [Shell; 3] = [Shell::GitBash, Shell::Powershell, Shell::Cmd];
    pub fn label(self) -> &'static str {
        match self {
            Shell::GitBash => "Git Bash",
            Shell::Powershell => "PowerShell",
            Shell::Cmd => "CMD",
        }
    }
}

/// Dónde abre una sesión suelta.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Target {
    #[default]
    Tab,
    Window,
    Here,
}

impl Target {
    pub const ALL: [Target; 3] = [Target::Tab, Target::Window, Target::Here];
    pub fn label(self) -> &'static str {
        match self {
            Target::Tab => "Pestaña nueva",
            Target::Window => "Ventana nueva",
            Target::Here => "Esta terminal",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SetLayout {
    #[default]
    Tabs,
    Panes,
    Windows,
}

impl SetLayout {
    pub const ALL: [SetLayout; 3] = [SetLayout::Tabs, SetLayout::Panes, SetLayout::Windows];
    pub fn label(self) -> &'static str {
        match self {
            SetLayout::Tabs => "Pestañas",
            SetLayout::Panes => "Paneles divididos",
            SetLayout::Windows => "Ventanas separadas",
        }
    }
    pub fn short(self) -> &'static str {
        match self {
            SetLayout::Tabs => "pestañas",
            SetLayout::Panes => "paneles",
            SetLayout::Windows => "ventanas",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SetWindow {
    #[default]
    New,
    Current,
}

impl SetWindow {
    pub const ALL: [SetWindow; 2] = [SetWindow::New, SetWindow::Current];
    pub fn label(self) -> &'static str {
        match self {
            SetWindow::New => "Ventana nueva",
            SetWindow::Current => "Ventana actual",
        }
    }
}

/// Qué sesión abre una entrada de set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum SessionRef {
    /// Sesión nueva desde cero cada vez.
    New,
    /// La sesión más reciente de esa carpeta, resuelta al abrir.
    Latest,
    Id(String),
}

impl From<String> for SessionRef {
    fn from(s: String) -> Self {
        match s.trim() {
            "new" | "" => SessionRef::New,
            "latest" => SessionRef::Latest,
            id => SessionRef::Id(id.to_string()),
        }
    }
}

impl From<SessionRef> for String {
    fn from(r: SessionRef) -> Self {
        match r {
            SessionRef::New => "new".into(),
            SessionRef::Latest => "latest".into(),
            SessionRef::Id(id) => id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct LaunchOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<Shell>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub fork: bool,
    /// `-n`: nombre visible de una sesión nueva.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `-w`: crear la sesión nueva en un worktree con este nombre.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SetEntry {
    /// Raíz del repo (para mostrar y para resolver "latest").
    pub repo: String,
    /// Carpeta de arranque si no es la raíz (subcarpeta o worktree).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub session: SessionRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_color: Option<String>,
    #[serde(default, flatten)]
    pub options: LaunchOptions,
}

impl SetEntry {
    pub fn launch_dir(&self) -> PathBuf {
        paths::normalize(&paths::expand(self.cwd.as_deref().unwrap_or(&self.repo)))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct SessionSet {
    pub name: String,
    #[serde(default)]
    pub layout: SetLayout,
    #[serde(default)]
    pub window: SetWindow,
    #[serde(default, rename = "entry")]
    pub entries: Vec<SetEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct General {
    /// Carpetas donde buscar repos (además de las que tienen sesiones).
    pub scan_roots: Vec<String>,
    pub scan_depth: usize,
    pub scan_exclude: Vec<String>,
    /// Ejecutable de Claude y argumentos previos; vacío = `claude` del PATH.
    pub claude_command: Vec<String>,
    /// Ruta a `bash.exe` de Git; vacío = autodetectar.
    pub git_bash: String,
    pub default_shell: Shell,
    pub default_target: Target,
    /// Al salir de Claude, deja la shell abierta en la carpeta.
    pub keep_shell_open: bool,
    pub exit_after_launch: bool,
    /// Perfil de Windows Terminal del que heredar icono y colores ("" = ninguno).
    pub wt_profile: String,
    pub tab_color: String,
    pub animations: bool,
}

impl Default for General {
    fn default() -> Self {
        General {
            scan_roots: ["~/Desktop", "~/Documents", "~/Downloads", "~/source", "~/Projects", "~/dev", "~/code", "~/repos"]
                .map(String::from)
                .to_vec(),
            scan_depth: 5,
            scan_exclude: [
                "node_modules", "AppData", "target", "dist", "build", "out", "venv", "__pycache__", "vendor",
                "bin", "obj", "packages", "Pods",
            ]
            .map(String::from)
            .to_vec(),
            claude_command: Vec::new(),
            git_bash: String::new(),
            default_shell: Shell::GitBash,
            default_target: Target::Tab,
            keep_shell_open: true,
            exit_after_launch: false,
            wt_profile: String::new(),
            tab_color: "#D97757".into(),
            animations: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Config {
    #[serde(default)]
    pub general: General,
    #[serde(default, rename = "set")]
    pub sets: Vec<SessionSet>,
}

const HEADER: &str = "\
# claunch — configuración y sets de sesiones.
# La TUI reescribe este archivo al editar sets (se pierden comentarios propios).
#
# [[set]] layout: \"tabs\" | \"panes\" | \"windows\"   window: \"new\" | \"current\"
# [[set.entry]] session: \"new\" | \"latest\" | \"<uuid>\"
#   shell: \"git-bash\" | \"powershell\" | \"cmd\"   model: \"opus\" | \"sonnet\" | ...
#   permission_mode: \"plan\" | \"acceptEdits\" | ...   fork = true   name / worktree (solo nuevas)

";

/// `CLAUNCH_CONFIG` permite otra ruta (pruebas, varias configuraciones).
pub fn path() -> PathBuf {
    match std::env::var_os("CLAUNCH_CONFIG") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => paths::config_dir().join("config.toml"),
    }
}

impl Config {
    /// Carga la config. Si el archivo existe pero es inválido, devuelve la
    /// config por defecto y el error: la app no debe sobrescribirlo.
    pub fn load() -> (Config, Option<String>) {
        let p = path();
        match std::fs::read_to_string(&p) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(c) => (c, None),
                Err(e) => {
                    let msg = format!("{} inválido: {}", p.display(), e.message());
                    crate::warn_log!("{msg}");
                    (Config::default(), Some(msg))
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Config::default(), None),
            Err(e) => {
                let msg = format!("no se pudo leer {}: {e}", p.display());
                crate::warn_log!("{msg}");
                (Config::default(), Some(msg))
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let body = toml::to_string_pretty(self)?;
        crate::util::write_atomic(&path(), format!("{HEADER}{body}").as_bytes())
    }

    pub fn scan_roots(&self) -> Vec<PathBuf> {
        self.general.scan_roots.iter().map(|r| paths::normalize(&paths::expand(r))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_sets() {
        let mut c = Config::default();
        c.sets.push(SessionSet {
            name: "Mi proyecto".into(),
            layout: SetLayout::Panes,
            window: SetWindow::New,
            entries: vec![
                SetEntry {
                    repo: r"c:\r\app".into(),
                    cwd: None,
                    session: SessionRef::Latest,
                    title: Some("App".into()),
                    tab_color: None,
                    options: LaunchOptions { model: Some("opus".into()), ..Default::default() },
                },
                SetEntry {
                    repo: r"c:\r\vm".into(),
                    cwd: Some(r"c:\r\vm\infra".into()),
                    session: SessionRef::Id("0f6c2b1e-5d4a-4c3b-9a8e-7d6f5e4c3b2a".into()),
                    title: None,
                    tab_color: Some("#7FB77E".into()),
                    options: LaunchOptions { fork: true, shell: Some(Shell::Powershell), ..Default::default() },
                },
            ],
        });
        let text = format!("{HEADER}{}", toml::to_string_pretty(&c).unwrap());
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, c);
        assert!(text.contains("session = \"latest\""));
    }

    #[test]
    fn partial_config_uses_defaults() {
        let c: Config = toml::from_str("[general]\nscan_depth = 2\n").unwrap();
        assert_eq!(c.general.scan_depth, 2);
        assert!(c.general.keep_shell_open);
        assert!(c.sets.is_empty());
    }
}
