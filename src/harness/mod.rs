//! Harness agénticos que claunch indexa y lanza: Claude Code, Codex, opencode,
//! Antigravity y Pi.
//!
//! Cada harness tiene su módulo con lo mismo: dónde guarda sus datos, cómo
//! listar e indexar sus sesiones y qué argumentos lleva una sesión nueva,
//! reanudada o bifurcada. `Harness` reparte las llamadas. Lo que cambia de uno
//! a otro (fork, nombre, modos de permiso…) se declara en [`Info`], y la
//! interfaz solo ofrece lo que el harness soporta.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::LaunchOptions;
use crate::data::SessionMeta;
use crate::launch::Mode;
use crate::paths;

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod opencode;
pub mod pi;
pub mod sqlite;

/// Sesiones del escaneo anterior de un harness, por clave de índice.
pub type Prev = HashMap<PathBuf, SessionMeta>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Harness {
    #[default]
    Claude,
    Codex,
    Opencode,
    Antigravity,
    Pi,
}

/// Cómo se elige el modelo al abrir una sesión.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelInput {
    /// Lista cerrada de alias; el primero, vacío, es «por defecto».
    Choice(&'static [&'static str]),
    /// Texto libre; lleva un ejemplo para mostrar como marcador.
    Free(&'static str),
    /// El harness no acepta modelo al arrancar.
    None,
}

/// Un modo de permisos: el valor que se guarda en la config, su rótulo y su ayuda.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Perm {
    pub value: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

/// Lo que distingue a un harness de cara a la interfaz y al arranque.
#[derive(Debug)]
pub struct Info {
    /// Identificador en la config y en la línea de comandos (`codex`).
    pub id: &'static str,
    /// Nombre completo («Claude Code»).
    pub label: &'static str,
    /// Rótulo corto de las filas (`agy`).
    pub tag: &'static str,
    /// Ejecutable que se busca en el PATH.
    pub program: &'static str,
    /// Cómo instalarlo (se muestra si falta).
    pub install: &'static str,
    /// Color del rótulo y de la pestaña, `#RRGGBB`.
    pub color: &'static str,
    /// Puede bifurcar una sesión existente desde la línea de comandos.
    pub fork: bool,
    /// Acepta un nombre al crear la sesión.
    pub name: bool,
    /// Puede crear la sesión en un git worktree nuevo.
    pub worktree: bool,
    pub model: ModelInput,
    /// Modos de permisos; el primero, de valor vacío, es «por defecto».
    pub permissions: &'static [Perm],
}

impl Harness {
    pub const ALL: [Harness; 5] = [Harness::Claude, Harness::Codex, Harness::Opencode, Harness::Antigravity, Harness::Pi];

    pub fn info(self) -> &'static Info {
        match self {
            Harness::Claude => &claude::INFO,
            Harness::Codex => &codex::INFO,
            Harness::Opencode => &opencode::INFO,
            Harness::Antigravity => &antigravity::INFO,
            Harness::Pi => &pi::INFO,
        }
    }

    pub fn id(self) -> &'static str {
        self.info().id
    }

    pub fn label(self) -> &'static str {
        self.info().label
    }

    pub fn tag(self) -> &'static str {
        self.info().tag
    }

    /// Harness por identificador, rótulo corto o ejecutable (`agy` = Antigravity).
    pub fn parse(s: &str) -> Option<Harness> {
        let s = s.trim().to_ascii_lowercase();
        Harness::ALL.into_iter().find(|h| {
            let i = h.info();
            s == i.id || s == i.tag || s == i.program
        })
    }

    pub fn is_claude(&self) -> bool {
        *self == Harness::Claude
    }

    /// Carpeta de datos por defecto (respeta las variables de entorno del harness).
    pub fn home(self) -> PathBuf {
        match self {
            Harness::Claude => claude::home(),
            Harness::Codex => codex::home(),
            Harness::Opencode => opencode::home(),
            Harness::Antigravity => antigravity::home(),
            Harness::Pi => pi::home(),
        }
    }

    /// Unidades que indexar: un archivo por sesión o, si el harness guarda todo
    /// en una base de datos, la base.
    pub fn units(self, home: &Path) -> Vec<PathBuf> {
        match self {
            Harness::Claude => claude::units(home),
            Harness::Codex => codex::units(home),
            Harness::Opencode => opencode::units(home),
            Harness::Antigravity => antigravity::units(home),
            Harness::Pi => pi::units(home),
        }
    }

    /// Indexa una unidad y devuelve sus sesiones con su clave de índice.
    /// `prev` trae las del escaneo anterior para leer solo lo que cambió.
    pub fn index(self, home: &Path, unit: &Path, prev: &Prev) -> Vec<(PathBuf, SessionMeta)> {
        match self {
            Harness::Claude => claude::index(unit, prev),
            Harness::Codex => codex::index(home, unit, prev),
            Harness::Opencode => opencode::index(home, unit, prev),
            Harness::Antigravity => antigravity::index(home, unit, prev),
            Harness::Pi => pi::index(home, unit, prev),
        }
    }

    /// Retoque tras indexar todas las unidades (datos que viven en otro
    /// archivo, como los títulos de Codex).
    pub fn finish(self, home: &Path, metas: &mut Prev) {
        match self {
            Harness::Codex => codex::finish(home, metas),
            Harness::Antigravity => antigravity::finish(home, metas),
            Harness::Claude | Harness::Opencode | Harness::Pi => {}
        }
    }

    /// Clave con la que `index` devolvió esta sesión (para rehacer el mapa
    /// desde el índice guardado).
    pub fn index_key(self, meta: &SessionMeta) -> PathBuf {
        match self {
            // Sus sesiones son filas de una base de datos: la clave es el id.
            Harness::Opencode | Harness::Antigravity => PathBuf::from(&meta.id),
            // Un archivo por sesión.
            Harness::Claude | Harness::Codex | Harness::Pi => meta.file.clone(),
        }
    }

    /// Argumentos del harness para un modo y opciones. Devuelve también el id
    /// de sesión si se conoce de antemano. `file` es el archivo de la sesión
    /// que se reanuda o bifurca (Pi lo necesita).
    pub fn args(self, mode: &Mode, opts: &LaunchOptions, file: Option<&Path>) -> (Vec<String>, Option<String>) {
        match self {
            Harness::Claude => claude::args(mode, opts),
            Harness::Codex => codex::args(mode, opts),
            Harness::Opencode => opencode::args(mode, opts),
            Harness::Antigravity => antigravity::args(mode, opts),
            Harness::Pi => pi::args(mode, opts, file),
        }
    }

    /// Comando para retomar la sesión a mano (se muestra al salir).
    pub fn resume_command(self, id: &str) -> String {
        match self {
            Harness::Claude => format!("claude -r {id}"),
            Harness::Codex => format!("codex resume {id}"),
            Harness::Opencode => format!("opencode -s {id}"),
            Harness::Antigravity => format!("agy --conversation {id}"),
            Harness::Pi => format!("pi --session {id}"),
        }
    }

    /// Carpeta con la grafía con la que se arranca el harness. Claude compara
    /// el `cwd` como texto y claunch lo arranca siempre con la unidad en
    /// minúscula (ver `paths`); el resto recibe la ruta como la da el sistema
    /// (`C:\Users\x`), se haya registrado como se haya registrado.
    pub fn launch_cwd(self, dir: &Path) -> PathBuf {
        match self {
            Harness::Claude => paths::normalize(dir),
            _ => paths::natural(dir),
        }
    }

    /// Plazo (ms) desde el arranque de un proceso en el que una sesión nueva
    /// sin id se toma por la suya aunque haya otros procesos que también
    /// pudieron crearla (ver `State::guess_sessions`). Codex fecha la sesión al
    /// arrancar, aunque no la guarde hasta el primer prompt: basta el tiempo
    /// que tarda en arrancar. opencode y Antigravity la crean con el primer
    /// prompt: se da el margen de quien abre una pestaña para usarla ya.
    pub fn first_session_window_ms(self) -> i64 {
        match self {
            Harness::Codex => 30_000,
            _ => 10 * 60_000,
        }
    }

    /// Ejecutable del harness: el del PATH o, si no está, el de sus
    /// ubicaciones habituales. Antigravity prefiere el que mantiene al día su
    /// actualizador (ver `antigravity::locate`).
    pub fn locate(self) -> Option<PathBuf> {
        let program = self.info().program;
        match self {
            Harness::Antigravity => antigravity::locate(),
            Harness::Codex => paths::which(program).or_else(codex::locate),
            _ => paths::which(program).or_else(|| paths::which_in(&self.install_dir(&paths::home())?, program)),
        }
    }

    /// Carpeta donde el instalador oficial deja el ejecutable. Solo la añade
    /// al PATH de las terminales nuevas, así que el proceso que acaba de
    /// instalarlo (o un lanzador gráfico) puede no tenerla.
    fn install_dir(self, home: &Path) -> Option<PathBuf> {
        match self {
            Harness::Claude => Some(home.join(".local").join("bin")),
            Harness::Opencode => Some(home.join(".opencode").join("bin")),
            _ => None,
        }
    }

    /// Último ajuste del comando, ya en la pestaña y justo antes de arrancar:
    /// quita lo que este ejecutable no admite. Devuelve el aviso para el usuario.
    pub fn preflight(self, program: &Path, args: &mut Vec<String>) -> Option<String> {
        match self {
            Harness::Antigravity => antigravity::preflight(program, args),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_parse_back() {
        for h in Harness::ALL {
            assert_eq!(Harness::parse(h.id()), Some(h));
            assert_eq!(Harness::parse(h.tag()), Some(h));
            // El id es el nombre con el que serde lo guarda en la config.
            assert_eq!(serde_json::to_string(&h).unwrap(), format!("\"{}\"", h.id()));
            let i = h.info();
            assert!(crate::ui::theme::parse_hex(i.color).is_some(), "{}", i.id);
            assert_eq!(i.permissions.first().map(|p| p.value), Some(""), "{}: el primer modo es «por defecto»", i.id);
        }
        assert_eq!(Harness::parse("AGY"), Some(Harness::Antigravity));
        assert_eq!(Harness::parse("gemini"), None);
    }

    #[test]
    fn official_install_folders_are_searched_when_not_in_path() {
        let home = std::env::temp_dir().join("claunch-tests").join(format!("install-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let exe = |name: &str| if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
        for (h, dir) in [(Harness::Claude, ".local"), (Harness::Opencode, ".opencode")] {
            let bin = home.join(dir).join("bin");
            assert_eq!(h.install_dir(&home), Some(bin.clone()));
            let program = h.info().program;
            assert_eq!(paths::which_in(&bin, program), None);
            std::fs::create_dir_all(&bin).unwrap();
            let file = bin.join(exe(program));
            std::fs::write(&file, "").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                // Sin permiso de ejecución no cuenta.
                assert_eq!(paths::which_in(&bin, program), None);
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            assert_eq!(paths::which_in(&bin, program), Some(file));
        }
        // Los que se instalan con npm o tienen su propia búsqueda no llevan carpeta fija aquí.
        assert!([Harness::Codex, Harness::Antigravity, Harness::Pi].iter().all(|h| h.install_dir(&home).is_none()));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn every_harness_builds_new_resume_and_fork_args() {
        let opts = LaunchOptions::default();
        let file = Path::new("sesion.jsonl");
        for h in Harness::ALL {
            let (_, id) = h.args(&Mode::Resume("abc".into()), &opts, Some(file));
            assert_eq!(id.as_deref(), Some("abc"), "{}", h.id());
            let (resume, _) = h.args(&Mode::Resume("abc".into()), &opts, Some(file));
            let (new, _) = h.args(&Mode::New, &opts, None);
            assert_ne!(resume, new, "{}: reanudar no puede ser igual que una sesión nueva", h.id());
            if h.info().fork {
                let (fork, id) = h.args(&Mode::Fork("abc".into()), &opts, Some(file));
                assert_ne!(id.as_deref(), Some("abc"), "{}: el fork estrena id", h.id());
                assert_ne!(fork, resume, "{}", h.id());
                assert_ne!(fork, new, "{}", h.id());
            }
        }
    }
}
