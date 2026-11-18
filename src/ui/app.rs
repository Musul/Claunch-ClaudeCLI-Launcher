//! Estado de la TUI y manejo de eventos.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tachyonfx::Motion;

use super::fx::Fx;
use super::widgets::{ListState, TextInput};
use crate::config::{Config, LaunchOptions, SessionRef, SessionSet, SetEntry, SetLayout, SetWindow, Shell, Target};
use crate::data::graph::{self, Graph};
use crate::data::{Aliases, Catalog, LiveSession, UsageState, WorkerCmd};
use crate::claude_settings::{self, BypassStatus};
use crate::harness::{Harness, ModelInput};
use crate::launch::{Env, LaunchItem, Mode, Placement};
use crate::{paths, util};

// ---------------------------------------------------------------- tipos

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Panel {
    Repos,
    Recents,
    Sets,
}

impl Panel {
    pub const ALL: [Panel; 3] = [Panel::Repos, Panel::Recents, Panel::Sets];
    pub fn title(self) -> &'static str {
        match self {
            Panel::Repos => "Repositorios",
            Panel::Recents => "Recientes",
            Panel::Sets => "Sets",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListId {
    Panel(Panel),
    Sessions,
    PickRepos,
    SetRows,
    MapNodes,
}

/// Región clicable registrada al dibujar.
#[derive(Clone, Debug)]
pub struct Hit {
    pub list: ListId,
    pub rect: Rect,
    /// `Some(i)`: la región es la fila `i`. `None`: lista uniforme.
    pub idx: Option<usize>,
    pub offset: usize,
    pub item_h: u16,
    pub count: usize,
}

#[derive(Default, Clone)]
pub struct Filter {
    pub input: TextInput,
    pub editing: bool,
}

impl Filter {
    pub fn text(&self) -> &str {
        &self.input.value
    }
    pub fn clear(&mut self) {
        self.input = TextInput::default();
        self.editing = false;
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PickCtx {
    pub set: usize,
    pub replace: Option<usize>,
}

pub enum Screen {
    Repo { key: String, list: ListState, filter: Filter, pick: Option<PickCtx> },
    PickRepo { list: ListState, filter: Filter, ctx: PickCtx },
    SetEditor { set: usize, row: usize, name: TextInput },
    /// Mapa de sesiones: `sel` es la clave del nodo elegido; `window`, índice en `MAP_WINDOWS`.
    Map { sel: Option<String>, window: usize },
}

/// Ventanas de tiempo del mapa: (milisegundos, rótulo).
pub const MAP_WINDOWS: [(i64, &str); 3] = [(3_600_000, "1 h"), (86_400_000, "24 h"), (7 * 86_400_000, "7 días")];
/// Nodos que caben en el mapa sin volverse ilegible.
pub const MAP_MAX_NODES: usize = 18;

struct GraphCache {
    at: Instant,
    window: usize,
    catalog: Arc<Catalog>,
    live: Arc<HashMap<String, LiveSession>>,
    graph: Arc<Graph>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessRow {
    New,
    Latest,
    Session(usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EdRow {
    Name,
    Layout,
    Window,
    Entry(usize),
    Add,
    Open,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FieldId {
    Target,
    Shell,
    /// Agente con el que se abre una sesión nueva.
    Harness,
    Mode,
    Model,
    Permission,
    Name,
    Worktree,
    Title,
    Color,
}

impl FieldId {
    /// Campos que cambian con el harness (se rehacen al elegir otro agente).
    fn depends_on_harness(self) -> bool {
        matches!(self, FieldId::Mode | FieldId::Model | FieldId::Permission | FieldId::Name | FieldId::Worktree)
    }
}

pub enum FieldKind {
    Cycle { opts: Vec<(String, String)>, idx: usize },
    Text { input: TextInput, placeholder: String },
}

pub struct Field {
    pub id: FieldId,
    pub label: &'static str,
    pub kind: FieldKind,
    /// Ayuda de cada opción de un ciclo, en su mismo orden (vacío: la del campo).
    pub hints: Vec<&'static str>,
}

impl Field {
    fn cycle(id: FieldId, label: &'static str, opts: Vec<(String, String)>, current: &str) -> Field {
        let idx = opts.iter().position(|(v, _)| v == current).unwrap_or(0);
        Field { id, label, kind: FieldKind::Cycle { opts, idx }, hints: Vec::new() }
    }
    fn text(id: FieldId, label: &'static str, value: &str, placeholder: &str) -> Field {
        Field { id, label, kind: FieldKind::Text { input: TextInput::new(value), placeholder: placeholder.into() }, hints: Vec::new() }
    }
    pub fn value(&self) -> String {
        match &self.kind {
            FieldKind::Cycle { opts, idx } => opts[*idx].0.clone(),
            FieldKind::Text { input, .. } => input.value.trim().to_string(),
        }
    }
    /// Texto de ayuda del valor actual.
    pub fn hint(&self) -> &'static str {
        if let FieldKind::Cycle { idx, .. } = &self.kind
            && let Some(h) = self.hints.get(*idx) {
                return h;
            }
        let v = self.value();
        match (self.id, v.as_str()) {
            (FieldId::Mode, "fork") => "copia nueva; la original queda intacta",
            (FieldId::Mode, _) => "continúa la misma sesión",
            (FieldId::Target, "here") => "reemplaza esta TUI hasta salir de la sesión",
            (FieldId::Harness, _) => "agente con el que se abre",
            (FieldId::Name, _) => "nombre visible de la sesión",
            (FieldId::Worktree, _) => "crea un git worktree aislado",
            (FieldId::Title, _) => "título fijo de la pestaña",
            _ => "",
        }
    }
}

/// Qué se abre o se guarda desde el modal de opciones.
pub enum OptPurpose {
    /// `existing`: id de la sesión que se reanuda o bifurca; `file`, su archivo.
    Launch { cwd: PathBuf, label: String, title: String, existing: Option<String>, file: Option<PathBuf> },
    Entry { ctx: PickCtx, repo: String, cwd: Option<String>, session: SessionRef },
}

pub struct OptionsModal {
    pub purpose: OptPurpose,
    pub heading: String,
    pub sub: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    /// Harness de la sesión: el suyo si ya existe; si es nueva, el del campo «Agente».
    pub harness: Harness,
}

impl OptionsModal {
    pub fn field(&self, id: FieldId) -> Option<&Field> {
        self.fields.iter().find(|f| f.id == id)
    }
    fn get(&self, id: FieldId) -> Option<String> {
        self.field(id).map(Field::value).filter(|v| !v.is_empty())
    }
    pub fn options(&self) -> LaunchOptions {
        LaunchOptions {
            shell: self.get(FieldId::Shell).map(|s| match s.as_str() {
                "powershell" => Shell::Powershell,
                "cmd" => Shell::Cmd,
                _ => Shell::GitBash,
            }),
            model: self.get(FieldId::Model),
            permission_mode: self.get(FieldId::Permission),
            fork: self.get(FieldId::Mode).as_deref() == Some("fork"),
            name: self.get(FieldId::Name),
            worktree: self.get(FieldId::Worktree),
        }
    }
    pub fn target(&self) -> Target {
        match self.get(FieldId::Target).as_deref() {
            Some("window") => Target::Window,
            Some("here") => Target::Here,
            _ => Target::Tab,
        }
    }
    /// Vista previa del comando (`claude …`, `codex …`).
    pub fn preview(&self) -> String {
        let opts = self.options();
        let (mode, file) = match &self.purpose {
            OptPurpose::Launch { existing: Some(id), file, .. } => {
                (if opts.fork { Mode::Fork(id.clone()) } else { Mode::Resume(id.clone()) }, file.as_deref())
            }
            OptPurpose::Launch { existing: None, .. } => (Mode::New, None),
            OptPurpose::Entry { session: SessionRef::New, .. } => (Mode::New, None),
            OptPurpose::Entry { session: SessionRef::Latest, .. } => {
                (if opts.fork { Mode::Fork("<última>".into()) } else { Mode::Resume("<última>".into()) }, None)
            }
            OptPurpose::Entry { session: SessionRef::Id(id), .. } => {
                (if opts.fork { Mode::Fork(id.clone()) } else { Mode::Resume(id.clone()) }, None)
            }
        };
        let (args, _) = self.harness.args(&mode, &opts, file);
        let mut out = String::from(self.harness.info().program);
        for a in args {
            let shown = if a.len() == 36 && a.is_ascii() && a.chars().filter(|c| *c == '-').count() == 4 {
                format!("{}…", &a[..8])
            } else if a.chars().count() > 30 && a.contains(['/', '\\']) {
                // Ruta larga (archivo de sesión): basta el nombre.
                let name = a.rsplit(['/', '\\']).next().unwrap_or(&a);
                format!("…{}{}", paths::SEP, util::truncate(name, 24))
            } else if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                a
            };
            out.push(' ');
            out.push_str(&shown);
        }
        out
    }
}

pub struct Planned {
    pub cwd: PathBuf,
    pub label: String,
    pub title: String,
    pub harness: Harness,
    /// Archivo de la sesión que se reanuda o bifurca (ver `Harness::args`).
    pub file: Option<PathBuf>,
    pub shell: Shell,
    pub tab_color: Option<String>,
    pub fixed_title: bool,
    pub mode: Mode,
    pub opts: LaunchOptions,
}

pub struct LaunchRequest {
    pub items: Vec<Planned>,
    pub placement: Placement,
    pub here: bool,
    pub label: String,
}

pub struct Conflict {
    pub item: usize,
    pub title: String,
    pub detail: String,
}

pub struct GuardModal {
    pub request: LaunchRequest,
    pub conflicts: Vec<Conflict>,
    pub choice: usize,
    /// Todas las sesiones en conflicto se pueden abrir como fork (depende del harness).
    pub can_fork: bool,
}

/// Qué hacer con una sesión que ya está abierta.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GuardChoice {
    Fork,
    OpenAnyway,
    Cancel,
}

impl GuardModal {
    /// Opciones que se ofrecen, en orden.
    pub fn choices(&self) -> &'static [GuardChoice] {
        if self.can_fork {
            &[GuardChoice::Fork, GuardChoice::OpenAnyway, GuardChoice::Cancel]
        } else {
            &[GuardChoice::OpenAnyway, GuardChoice::Cancel]
        }
    }
}

pub enum ConfirmAction {
    DeleteSet(usize),
    RemoveEntry(usize, usize),
    /// Activar (`true`) o desactivar el bypass global de permisos.
    Bypass(bool),
}

pub struct ConfirmModal {
    pub message: String,
    pub detail: String,
    pub action: ConfirmAction,
    pub yes: bool,
}

pub enum Modal {
    Options(OptionsModal),
    Guard(GuardModal),
    Confirm(ConfirmModal),
    Help,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToastKind {
    Info,
    Success,
    Warn,
    Error,
}

pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub born: Instant,
    pub ttl: Duration,
}

struct Batch {
    label: String,
    tokens: Vec<String>,
    confirmed: usize,
    born: Instant,
}

pub struct HomeState {
    pub focus: Panel,
    pub repos: ListState,
    pub recents: ListState,
    pub sets: ListState,
    pub filter: Filter,
}

const RECENTS: usize = 40;
const RECENT_ACTIVITY_MS: i64 = 90_000;

pub struct App {
    pub cfg: Config,
    pub cfg_error: Option<String>,
    pub env: Result<Env, String>,
    pub catalog: Arc<Catalog>,
    pub live: Arc<HashMap<String, LiveSession>>,
    pub aliases: Arc<Aliases>,
    /// Uso del plan de Claude (barra de la cabecera).
    pub usage: UsageState,
    graph_cache: Option<GraphCache>,
    pub progress: Option<(usize, usize)>,
    pub loaded: bool,
    pub home: HomeState,
    /// Agente de «Nueva sesión» (⇥ lo cambia); ver `new_session_harness`.
    pub new_harness: Harness,
    pub stack: Vec<Screen>,
    pub modal: Option<Modal>,
    pub toasts: Vec<Toast>,
    batches: Vec<Batch>,
    last_poll: Instant,
    pub fx: Fx,
    pub hits: Vec<Hit>,
    pub flash_rect: Option<Rect>,
    pub needs_tick: bool,
    pub quit: bool,
    pub run_here: Option<LaunchItem>,
    pub worker: Option<Sender<WorkerCmd>>,
    pub started: Instant,
    last_click: Option<(Instant, ListId, usize)>,
    /// Estado del bypass global de permisos (`~/.claude/settings.json`).
    pub bypass: Result<BypassStatus, String>,
    bypass_checked: Instant,
    /// Sin escrituras en config (modo snapshot).
    pub read_only: bool,
    /// Datos de ejemplo (`claunch demo`): nada se lanza ni se escribe.
    pub demo: bool,
    demo_epoch: u64,
    /// Si es `Some`, los lanzamientos se registran aquí en vez de ejecutarse (tests).
    pub dry_run: Option<Vec<(Vec<LaunchItem>, Placement, bool)>>,
}

impl App {
    pub fn new(cfg: Config, cfg_error: Option<String>, worker: Option<Sender<WorkerCmd>>) -> App {
        let env = Env::resolve(&cfg).map_err(|e| format!("{e:#}"));
        let animations = cfg.general.animations && std::env::var_os("CLAUNCH_NO_ANIM").is_none();
        let mut app = App {
            env,
            new_harness: cfg.general.default_harness,
            cfg_error,
            catalog: Arc::new(Catalog::default()),
            live: Arc::new(HashMap::new()),
            aliases: Arc::new(Aliases::new()),
            usage: if cfg.general.show_usage { UsageState::Loading } else { UsageState::Disabled },
            graph_cache: None,
            progress: None,
            loaded: false,
            home: HomeState {
                focus: Panel::Repos,
                repos: ListState::default(),
                recents: ListState::default(),
                sets: ListState::default(),
                filter: Filter::default(),
            },
            stack: Vec::new(),
            modal: None,
            toasts: Vec::new(),
            batches: Vec::new(),
            last_poll: Instant::now(),
            fx: Fx::new(animations),
            hits: Vec::new(),
            flash_rect: None,
            needs_tick: false,
            quit: false,
            run_here: None,
            worker,
            started: Instant::now(),
            last_click: None,
            bypass: claude_settings::status().map_err(|e| format!("{e:#}")),
            bypass_checked: Instant::now(),
            read_only: false,
            demo: false,
            demo_epoch: 0,
            dry_run: None,
            cfg,
        };
        if let Some(e) = app.cfg_error.clone() {
            app.toast(ToastKind::Error, format!("{e} — sets en solo lectura"));
        }
        match app.env.clone() {
            Err(e) => app.toast(ToastKind::Error, e),
            // Un agente mal configurado no impide usar los demás, pero se avisa.
            Ok(env) => {
                for (h, problem) in &env.problems {
                    app.toast(ToastKind::Error, format!("{} mal configurado: {problem}", h.label()));
                }
            }
        }
        app
    }

    // ------------------------------------------------------------ utilidades

    pub fn toast(&mut self, kind: ToastKind, text: impl Into<String>) {
        let text = text.into();
        let ttl = match kind {
            ToastKind::Error => Duration::from_secs(9),
            ToastKind::Warn => Duration::from_secs(7),
            _ => Duration::from_secs(4),
        };
        if matches!(kind, ToastKind::Error | ToastKind::Warn) {
            crate::warn_log!("{text}");
        }
        self.toasts.retain(|t| t.text != text);
        self.toasts.push(Toast { text, kind, born: Instant::now(), ttl });
        if self.toasts.len() > 4 {
            self.toasts.remove(0);
        }
        self.fx.toast = true;
    }

    fn save_config(&mut self) -> bool {
        if self.read_only {
            return true;
        }
        if self.cfg_error.is_some() {
            self.toast(ToastKind::Warn, "config.toml inválido: no se guardan cambios hasta corregirlo");
            return false;
        }
        match self.cfg.save() {
            Ok(()) => true,
            Err(e) => {
                self.toast(ToastKind::Error, format!("no se pudo guardar la config: {e:#}"));
                false
            }
        }
    }

    pub fn now_ms(&self) -> i64 {
        util::now_ms()
    }

    pub fn screen_name(&self) -> Vec<String> {
        let mut v = vec!["Inicio".to_string()];
        for s in &self.stack {
            match s {
                Screen::Repo { key, pick, .. } => {
                    let name = self.catalog.repo(key).map(|r| r.name.clone()).unwrap_or_else(|| "repo".into());
                    if pick.is_some() {
                        v.push("elegir sesión".into());
                    }
                    v.push(name);
                }
                Screen::PickRepo { ctx, .. } => {
                    v.push(self.set_name(ctx.set));
                    v.push("elegir repo".into());
                }
                Screen::SetEditor { set, .. } => v.push(format!("Set: {}", self.set_name(*set))),
                Screen::Map { .. } => v.push("Mapa de sesiones".into()),
            }
        }
        v
    }

    fn set_name(&self, i: usize) -> String {
        self.cfg.sets.get(i).map(|s| s.name.clone()).unwrap_or_default()
    }

    fn default_shell(&self) -> Shell {
        self.cfg.general.default_shell
    }

    /// Harness que se pueden lanzar: activos en la config y con ejecutable.
    pub fn installed(&self) -> Vec<Harness> {
        self.env.as_ref().map(Env::installed).unwrap_or_default()
    }

    /// Agente de «Nueva sesión»: el elegido si se puede lanzar; si no, el
    /// primero disponible (o el elegido, para que el fallo diga cuál falta).
    pub fn new_session_harness(&self) -> Harness {
        let installed = self.installed();
        if installed.is_empty() || installed.contains(&self.new_harness) { self.new_harness } else { installed[0] }
    }

    /// Palabra con la que el filtro encuentra las sesiones de un harness
    /// («codex» deja solo las suyas). Con un solo harness no aporta nada.
    fn harness_word(&self, h: Harness) -> &'static str {
        if self.catalog.mixed() { h.tag() } else { "" }
    }

    /// Pasa al agente siguiente (o anterior) para «Nueva sesión».
    fn cycle_new_harness(&mut self, d: isize) {
        let installed = self.installed();
        if installed.len() > 1 {
            self.new_harness = cycle(&installed, self.new_session_harness(), d);
        }
    }

    /// Color de pestaña por defecto para un harness: el de `general.tab_color`
    /// si el usuario lo cambió; si no, el del harness.
    pub fn tab_color(&self, h: Harness) -> String {
        let general = &self.cfg.general.tab_color;
        if general.eq_ignore_ascii_case(crate::config::DEFAULT_TAB_COLOR) { h.info().color.to_string() } else { general.clone() }
    }

    /// Harness con el que se abrirá una entrada de set: el de su sesión si es
    /// una concreta (manda sobre lo que diga la entrada); si no, el de la entrada.
    pub fn entry_harness(&self, e: &SetEntry) -> Harness {
        match &e.session {
            SessionRef::Id(id) => self.catalog.session(id).map_or(e.harness, |s| s.meta.harness),
            _ => e.harness,
        }
    }

    /// Color de pestaña de una entrada de set: el suyo o, sin él, el de su harness.
    pub fn entry_color(&self, e: &SetEntry) -> String {
        e.tab_color.clone().unwrap_or_else(|| self.tab_color(self.entry_harness(e)))
    }

    // ------------------------------------------------------------ filas

    pub fn repo_rows(&self, filter: &str) -> Vec<usize> {
        (0..self.catalog.repos.len())
            .filter(|&i| {
                let r = &self.catalog.repos[i];
                filter.is_empty() || util::matches(filter, &format!("{} {}", r.name, r.root.display()))
            })
            .collect()
    }

    pub fn recent_rows(&self, filter: &str) -> Vec<usize> {
        let mut out = Vec::new();
        for (i, s) in self.catalog.sessions.iter().enumerate() {
            if out.len() >= RECENTS {
                break;
            }
            let repo = self.catalog.repo(&s.repo_key).map(|r| r.name.as_str()).unwrap_or("");
            let hay = format!("{} {} {} {}", s.meta.title(), repo, s.meta.git_branch.as_deref().unwrap_or(""), self.harness_word(s.meta.harness));
            if filter.is_empty() || util::matches(filter, &hay) {
                out.push(i);
            }
        }
        out
    }

    pub fn set_rows(&self, filter: &str) -> Vec<Option<usize>> {
        let mut v: Vec<Option<usize>> = (0..self.cfg.sets.len())
            .filter(|&i| filter.is_empty() || util::matches(filter, &self.cfg.sets[i].name))
            .map(Some)
            .collect();
        if filter.is_empty() {
            v.push(None);
        }
        v
    }

    pub fn session_rows(&self, key: &str, filter: &str, pick: bool) -> Vec<SessRow> {
        let mut v = vec![SessRow::New];
        if pick {
            v.push(SessRow::Latest);
        }
        if let Some(repo) = self.catalog.repo(key) {
            for &i in &repo.sessions {
                let s = &self.catalog.sessions[i];
                let hay = format!(
                    "{} {} {} {} {}",
                    s.meta.title(),
                    s.meta.git_branch.as_deref().unwrap_or(""),
                    s.meta.last_prompt.as_deref().unwrap_or(""),
                    s.subpath.as_deref().unwrap_or(""),
                    self.harness_word(s.meta.harness)
                );
                if filter.is_empty() || util::matches(filter, &hay) {
                    v.push(SessRow::Session(i));
                }
            }
        }
        v
    }

    pub fn editor_rows(&self, set: usize) -> Vec<EdRow> {
        let mut v = vec![EdRow::Name, EdRow::Layout, EdRow::Window];
        let n = self.cfg.sets.get(set).map(|s| s.entries.len()).unwrap_or(0);
        v.extend((0..n).map(EdRow::Entry));
        v.push(EdRow::Add);
        v.push(EdRow::Open);
        v
    }

    fn panel_keys(&self, p: Panel, filter: &str) -> Vec<String> {
        match p {
            Panel::Repos => self.repo_rows(filter).into_iter().map(|i| self.catalog.repos[i].key.clone()).collect(),
            Panel::Recents => self.recent_rows(filter).into_iter().map(|i| self.catalog.sessions[i].meta.id.clone()).collect(),
            Panel::Sets => self.set_rows(filter).into_iter().map(|i| i.map(|i| i.to_string()).unwrap_or_else(|| "+".into())).collect(),
        }
    }

    pub fn sess_keys(&self, rows: &[SessRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                SessRow::New => "new".into(),
                SessRow::Latest => "latest".into(),
                SessRow::Session(i) => self.catalog.sessions[*i].meta.id.clone(),
            })
            .collect()
    }

    /// Reubica selecciones tras un refresco del catálogo.
    pub fn sync_lists(&mut self) {
        let f = self.home.filter.text().to_string();
        for p in Panel::ALL {
            let filter = if p == self.home.focus { f.as_str() } else { "" };
            let keys = self.panel_keys(p, filter);
            self.home_list(p).sync(&keys);
        }
        let mut stack = std::mem::take(&mut self.stack);
        for s in &mut stack {
            match s {
                Screen::Repo { key, list, filter, pick } => {
                    let rows = self.session_rows(key, filter.text(), pick.is_some());
                    list.sync(&self.sess_keys(&rows));
                }
                Screen::PickRepo { list, filter, .. } => {
                    let keys: Vec<String> = self.repo_rows(filter.text()).into_iter().map(|i| self.catalog.repos[i].key.clone()).collect();
                    list.sync(&keys);
                }
                Screen::SetEditor { .. } | Screen::Map { .. } => {}
            }
        }
        self.stack = stack;
    }

    /// Grafo de sesiones de la ventana `window`, recalculado como mucho cada segundo
    /// (o al llegar sesiones vivas o catálogo nuevos).
    pub fn graph(&mut self, window: usize) -> Arc<Graph> {
        if let Some(c) = &self.graph_cache
            && c.window == window
            && c.at.elapsed() < Duration::from_secs(1)
            && Arc::ptr_eq(&c.catalog, &self.catalog)
            && Arc::ptr_eq(&c.live, &self.live)
        {
            return c.graph.clone();
        }
        let ms = MAP_WINDOWS[window.min(MAP_WINDOWS.len() - 1)].0;
        let g = Arc::new(graph::build(&self.catalog, &self.live, &self.aliases, self.now_ms(), ms, MAP_MAX_NODES));
        self.graph_cache = Some(GraphCache {
            at: Instant::now(),
            window,
            catalog: self.catalog.clone(),
            live: self.live.clone(),
            graph: g.clone(),
        });
        g
    }

    fn open_map(&mut self) {
        self.push(Screen::Map { sel: None, window: 1 });
    }

    pub fn home_list(&mut self, p: Panel) -> &mut ListState {
        match p {
            Panel::Repos => &mut self.home.repos,
            Panel::Recents => &mut self.home.recents,
            Panel::Sets => &mut self.home.sets,
        }
    }

    // ------------------------------------------------------------ eventos

    pub fn tick(&mut self) {
        let now = Instant::now();
        self.toasts.retain(|t| now.duration_since(t.born) < t.ttl);
        if now.duration_since(self.last_poll) >= Duration::from_millis(250) && !self.batches.is_empty() {
            self.last_poll = now;
            self.poll_batches();
        }
        // Demo: renueva los datos para que los mensajes «en vivo» sigan frescos.
        if self.demo {
            let epoch = self.started.elapsed().as_secs() / 30;
            if epoch != self.demo_epoch {
                self.demo_epoch = epoch;
                super::demo::refresh(self);
            }
            return;
        }
        // settings.json puede cambiar fuera (Claude, VS Code, a mano).
        if now.duration_since(self.bypass_checked) >= Duration::from_secs(3) {
            self.bypass_checked = now;
            self.bypass = claude_settings::status().map_err(|e| format!("{e:#}"));
        }
    }

    fn ask_bypass(&mut self) {
        if self.demo {
            self.toast(ToastKind::Info, "Modo demo: el bypass global no se toca");
            return;
        }
        // El bypass global es un ajuste de Claude Code: sin él no hay nada que cambiar.
        if !self.installed().contains(&Harness::Claude) && !claude_settings::settings_path().exists() {
            self.toast(ToastKind::Info, "El bypass global es de Claude Code, que no está instalado");
            return;
        }
        match &self.bypass {
            Ok(s) if s.enabled => self.open_modal(Modal::Confirm(ConfirmModal {
                message: "¿Desactivar el bypass global de permisos?".into(),
                detail: "Claude volverá a pedir permiso antes de ejecutar herramientas en todas las sesiones nuevas. \
                         Se restaura el modo anterior y se respalda settings.json."
                    .into(),
                action: ConfirmAction::Bypass(false),
                yes: false,
            })),
            Ok(s) => {
                let policy = if s.blocked_by_policy { " Ojo: una política gestionada lo bloquea y no tendrá efecto." } else { "" };
                self.open_modal(Modal::Confirm(ConfirmModal {
                    message: "¿Activar bypass de permisos en TODAS las sesiones?".into(),
                    detail: format!(
                        "Escribe permissions.defaultMode = \"bypassPermissions\" en ~/.claude/settings.json. \
                         Claude ejecutará comandos, ediciones y llamadas de red sin preguntar, también en repos de \
                         producción. Para una sola sesión es más seguro usar o → Permisos.{policy}"
                    ),
                    action: ConfirmAction::Bypass(true),
                    yes: false,
                }))
            }
            Err(e) => {
                let e = e.clone();
                self.toast(ToastKind::Error, format!("No puedo leer settings.json: {e}"));
            }
        }
    }

    fn apply_bypass(&mut self, on: bool) {
        match claude_settings::set_bypass(on) {
            Ok(backup) => {
                self.bypass = claude_settings::status().map_err(|e| format!("{e:#}"));
                let state = if on { "activado" } else { "desactivado" };
                let b = backup.map(|b| format!(" · respaldo {}", paths::display(&b))).unwrap_or_default();
                let kind = if on { ToastKind::Warn } else { ToastKind::Success };
                self.toast(kind, format!("Bypass global {state} (aplica a sesiones nuevas){b}"));
            }
            Err(e) => self.toast(ToastKind::Error, format!("No se cambió settings.json: {e:#}")),
        }
    }

    fn poll_batches(&mut self) {
        let dir = paths::launch_dir();
        let mut done = Vec::new();
        for (bi, b) in self.batches.iter_mut().enumerate() {
            b.tokens.retain(|t| {
                let marker = dir.join(format!("{t}.started"));
                if marker.exists() {
                    let _ = std::fs::remove_file(&marker);
                    b.confirmed += 1;
                    false
                } else {
                    true
                }
            });
            if b.tokens.is_empty() || b.born.elapsed() > Duration::from_secs(15) {
                done.push(bi);
            }
        }
        for bi in done.into_iter().rev() {
            let b = self.batches.remove(bi);
            let total = b.confirmed + b.tokens.len();
            if b.tokens.is_empty() {
                let msg = if total == 1 { format!("{} arrancó", b.label) } else { format!("{}: {total}/{total} sesiones arrancaron", b.label) };
                self.toast(ToastKind::Success, msg);
                self.fx.shimmer();
                if self.cfg.general.exit_after_launch {
                    self.quit = true;
                }
            } else {
                self.toast(
                    ToastKind::Warn,
                    format!(
                        "{}: {}/{total} sin confirmar tras 15 s — revisa {}",
                        b.label,
                        b.tokens.len(),
                        if cfg!(windows) { "Windows Terminal" } else { "la terminal (general.terminal)" }
                    ),
                );
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            if self.modal.take().is_none() {
                self.quit = true;
            }
            return;
        }
        if let Some(modal) = self.modal.take() {
            self.modal_key(modal, key);
            return;
        }
        match self.stack.pop() {
            None => self.home_key(key),
            Some(screen) => self.screen_key(screen, key),
        }
    }

    fn filter_key(filter: &mut Filter, key: &KeyEvent) -> Option<bool> {
        // Some(true) = consumida y el filtro cambió; Some(false) = consumida; None = no.
        if !filter.editing {
            return None;
        }
        match key.code {
            KeyCode::Esc => {
                filter.clear();
                Some(true)
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown | KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab => None,
            KeyCode::Backspace if filter.input.value.is_empty() => {
                filter.editing = false;
                Some(false)
            }
            _ => filter.input.handle(key).then_some(true),
        }
    }

    fn home_key(&mut self, key: KeyEvent) {
        let focus = self.home.focus;
        if let Some(changed) = Self::filter_key(&mut self.home.filter, &key) {
            if changed {
                let keys = self.panel_keys(focus, &self.home.filter.input.value.clone());
                self.home_list(focus).select(0, &keys);
            }
            return;
        }
        let filter = self.home.filter.text().to_string();
        let keys = self.panel_keys(focus, &filter);
        let page = 8;
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.open_modal(Modal::Help),
            KeyCode::Char('r') | KeyCode::F(5) => self.refresh(),
            KeyCode::Char('/') => self.home.filter.editing = true,
            KeyCode::Esc if !filter.is_empty() => self.home.filter.clear(),
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => self.cycle_panel(1),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => self.cycle_panel(-1),
            KeyCode::Char('1') => self.focus_panel(Panel::Repos),
            KeyCode::Char('2') => self.focus_panel(Panel::Recents),
            KeyCode::Char('3') => self.focus_panel(Panel::Sets),
            KeyCode::Up | KeyCode::Char('k') => self.home_list(focus).step(-1, &keys),
            KeyCode::Down | KeyCode::Char('j') => self.home_list(focus).step(1, &keys),
            KeyCode::PageUp => self.home_list(focus).step(-page, &keys),
            KeyCode::PageDown => self.home_list(focus).step(page, &keys),
            KeyCode::Home | KeyCode::Char('g') => self.home_list(focus).select(0, &keys),
            KeyCode::End | KeyCode::Char('G') => self.home_list(focus).select(usize::MAX, &keys),
            KeyCode::Enter => self.activate_home(false),
            KeyCode::Char('o') => self.activate_home(true),
            KeyCode::Char('n') => self.new_set(),
            KeyCode::Char('m') => self.open_map(),
            KeyCode::Char('B') => self.ask_bypass(),
            KeyCode::Char('e') if focus == Panel::Sets => {
                if let Some(i) = self.selected_set() {
                    self.push(Screen::SetEditor { set: i, row: 0, name: TextInput::new(&self.cfg.sets[i].name) });
                }
            }
            KeyCode::Delete | KeyCode::Char('d') if focus == Panel::Sets => {
                if let Some(i) = self.selected_set() {
                    self.open_modal(Modal::Confirm(ConfirmModal {
                        message: format!("¿Borrar el set «{}»?", self.cfg.sets[i].name),
                        detail: "Solo se borra el set; las sesiones no se tocan.".into(),
                        action: ConfirmAction::DeleteSet(i),
                        yes: false,
                    }));
                }
            }
            _ => {}
        }
    }

    fn selected_set(&self) -> Option<usize> {
        let rows = self.set_rows(self.home.filter.text());
        rows.get(self.home.sets.idx).copied().flatten()
    }

    fn focus_panel(&mut self, p: Panel) {
        if self.home.focus != p {
            self.home.focus = p;
            self.home.filter.clear();
        }
    }

    fn cycle_panel(&mut self, d: isize) {
        let i = Panel::ALL.iter().position(|p| *p == self.home.focus).unwrap_or(0) as isize;
        let n = Panel::ALL.len() as isize;
        self.focus_panel(Panel::ALL[((i + d).rem_euclid(n)) as usize]);
    }

    fn activate_home(&mut self, with_options: bool) {
        let filter = self.home.filter.text().to_string();
        self.home.filter.editing = false;
        match self.home.focus {
            Panel::Repos => {
                let rows = self.repo_rows(&filter);
                if let Some(&i) = rows.get(self.home.repos.idx) {
                    let key = self.catalog.repos[i].key.clone();
                    self.open_repo(key, None);
                }
            }
            Panel::Recents => {
                let rows = self.recent_rows(&filter);
                if let Some(&i) = rows.get(self.home.recents.idx) {
                    if with_options { self.options_for_session(i) } else { self.launch_session(i) }
                }
            }
            Panel::Sets => match self.set_rows(&filter).get(self.home.sets.idx).copied() {
                Some(Some(i)) if with_options => {
                    self.push(Screen::SetEditor { set: i, row: 0, name: TextInput::new(&self.cfg.sets[i].name) })
                }
                Some(Some(i)) => self.launch_set(i),
                Some(None) => self.new_set(),
                None => {}
            },
        }
    }

    fn open_repo(&mut self, key: String, pick: Option<PickCtx>) {
        let mut list = ListState::default();
        let rows = self.session_rows(&key, "", pick.is_some());
        list.select(0, &self.sess_keys(&rows));
        self.push(Screen::Repo { key, list, filter: Filter::default(), pick });
    }

    fn push(&mut self, s: Screen) {
        self.stack.push(s);
        self.fx.screen = Some(Motion::LeftToRight);
    }

    fn pop(&mut self) {
        self.fx.screen = Some(Motion::RightToLeft);
    }

    fn open_modal(&mut self, m: Modal) {
        self.modal = Some(m);
        self.fx.modal = true;
    }

    fn refresh(&mut self) {
        if self.demo {
            self.toast(ToastKind::Info, "Modo demo: datos de ejemplo");
            return;
        }
        // También se vuelve a mirar qué agentes hay (por si se instaló uno).
        if self.dry_run.is_none() {
            self.env = Env::resolve(&self.cfg).map_err(|e| format!("{e:#}"));
        }
        if let Some(w) = &self.worker
            && w.send(WorkerCmd::Refresh).is_ok() {
                self.toast(ToastKind::Info, "Reescaneando repos y sesiones…");
                return;
            }
        self.toast(ToastKind::Error, "el indexador no responde");
    }

    /// Maneja una pantalla sacada de la pila; la vuelve a poner salvo que se cierre.
    fn screen_key(&mut self, mut screen: Screen, key: KeyEvent) {
        // Lo que la acción apile queda encima de la pantalla activa.
        let base = self.stack.len();
        let keep = match &mut screen {
            Screen::Repo { .. } => self.repo_key(&mut screen, key),
            Screen::PickRepo { .. } => self.pick_key(&mut screen, key),
            Screen::SetEditor { .. } => self.editor_key(&mut screen, key),
            Screen::Map { .. } => self.map_key(&mut screen, key),
        };
        if keep {
            self.stack.insert(base.min(self.stack.len()), screen);
        } else {
            self.pop();
        }
    }

    fn repo_key(&mut self, screen: &mut Screen, key: KeyEvent) -> bool {
        let Screen::Repo { key: repo_key, list, filter, pick } = screen else { return true };
        if let Some(changed) = Self::filter_key(filter, &key) {
            if changed {
                let rows = self.session_rows(repo_key, filter.text(), pick.is_some());
                list.select(0, &self.sess_keys(&rows));
            }
            return true;
        }
        let rows = self.session_rows(repo_key, filter.text(), pick.is_some());
        let keys = self.sess_keys(&rows);
        match key.code {
            KeyCode::Esc if !filter.text().is_empty() => filter.clear(),
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => return false,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.open_modal(Modal::Help),
            KeyCode::Char('/') => filter.editing = true,
            KeyCode::Char('r') | KeyCode::F(5) => self.refresh(),
            // Agente de «Nueva sesión».
            KeyCode::Tab => self.cycle_new_harness(1),
            KeyCode::BackTab => self.cycle_new_harness(-1),
            KeyCode::Up | KeyCode::Char('k') => list.step(-1, &keys),
            KeyCode::Down | KeyCode::Char('j') => list.step(1, &keys),
            KeyCode::PageUp => list.step(-8, &keys),
            KeyCode::PageDown => list.step(8, &keys),
            KeyCode::Home | KeyCode::Char('g') => list.select(0, &keys),
            KeyCode::End | KeyCode::Char('G') => list.select(usize::MAX, &keys),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('o') => {
                filter.editing = false;
                let with_options = key.code == KeyCode::Char('o');
                let row = rows.get(list.idx).copied();
                let repo_key = repo_key.clone();
                match (row, *pick) {
                    (Some(r), Some(ctx)) => {
                        self.entry_options(ctx, &repo_key, r);
                    }
                    (Some(SessRow::New), None) => {
                        if with_options { self.options_for_new(&repo_key) } else { self.launch_new(&repo_key) }
                    }
                    (Some(SessRow::Session(i)), None) => {
                        if with_options { self.options_for_session(i) } else { self.launch_session(i) }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        true
    }

    fn map_key(&mut self, screen: &mut Screen, key: KeyEvent) -> bool {
        let Screen::Map { sel, window } = screen else { return true };
        let g = self.graph(*window);
        let n = g.nodes.len();
        let cur = sel.as_deref().and_then(|k| g.index_of(k));
        let mut step = |d: isize| {
            if n > 0 {
                let i = match cur {
                    Some(i) => (i as isize + d).rem_euclid(n as isize) as usize,
                    None => 0,
                };
                *sel = Some(g.nodes[i].key.clone());
            }
        };
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => return false,
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.open_modal(Modal::Help),
            KeyCode::Char('r') | KeyCode::F(5) => self.refresh(),
            KeyCode::Up | KeyCode::Left | KeyCode::Char('k') | KeyCode::Char('h') | KeyCode::BackTab => step(-1),
            KeyCode::Down | KeyCode::Right | KeyCode::Char('j') | KeyCode::Char('l') | KeyCode::Tab => step(1),
            KeyCode::Char('t') => {
                *window = (*window + 1) % MAP_WINDOWS.len();
                self.graph_cache = None;
            }
            KeyCode::Enter => {
                let id = cur.and_then(|i| g.nodes[i].session_id.clone());
                match id.as_deref().and_then(|id| self.catalog.by_id.get(id).copied()) {
                    Some(si) => {
                        let repo_key = self.catalog.sessions[si].repo_key.clone();
                        self.open_repo(repo_key.clone(), None);
                        let rows = self.session_rows(&repo_key, "", false);
                        let keys = self.sess_keys(&rows);
                        if let Some(pos) = rows.iter().position(|r| *r == SessRow::Session(si))
                            && let Some(Screen::Repo { list, .. }) = self.stack.last_mut()
                        {
                            list.select(pos, &keys);
                        }
                    }
                    None if cur.is_some() => self.toast(ToastKind::Info, "Esa sesión no está en este equipo"),
                    None => {}
                }
            }
            _ => {}
        }
        true
    }

    fn pick_key(&mut self, screen: &mut Screen, key: KeyEvent) -> bool {
        let Screen::PickRepo { list, filter, ctx } = screen else { return true };
        if let Some(changed) = Self::filter_key(filter, &key) {
            if changed {
                let keys: Vec<String> = self.repo_rows(filter.text()).into_iter().map(|i| self.catalog.repos[i].key.clone()).collect();
                list.select(0, &keys);
            }
            return true;
        }
        let rows = self.repo_rows(filter.text());
        let keys: Vec<String> = rows.iter().map(|&i| self.catalog.repos[i].key.clone()).collect();
        match key.code {
            KeyCode::Esc if !filter.text().is_empty() => filter.clear(),
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left => return false,
            KeyCode::Char('/') => filter.editing = true,
            KeyCode::Up | KeyCode::Char('k') => list.step(-1, &keys),
            KeyCode::Down | KeyCode::Char('j') => list.step(1, &keys),
            KeyCode::PageUp => list.step(-8, &keys),
            KeyCode::PageDown => list.step(8, &keys),
            KeyCode::Enter | KeyCode::Right => {
                filter.editing = false;
                if let Some(k) = keys.get(list.idx).cloned() {
                    let ctx = *ctx;
                    self.open_repo(k, Some(ctx));
                }
            }
            _ => {}
        }
        true
    }

    fn editor_key(&mut self, screen: &mut Screen, key: KeyEvent) -> bool {
        let Screen::SetEditor { set, row, name } = screen else { return true };
        let set = *set;
        if set >= self.cfg.sets.len() {
            return false;
        }
        let rows = self.editor_rows(set);
        *row = (*row).min(rows.len() - 1);
        let current = rows[*row];
        let shift = key.modifiers.contains(KeyModifiers::SHIFT) || key.modifiers.contains(KeyModifiers::ALT);

        if current == EdRow::Name {
            match key.code {
                KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter | KeyCode::Esc => {
                    self.commit_set_name(set, name);
                }
                _ => {
                    name.handle(&key);
                    return true;
                }
            }
        }
        match key.code {
            KeyCode::Esc => return false,
            KeyCode::Up if shift => self.move_entry(set, row, &rows, -1),
            KeyCode::Down if shift => self.move_entry(set, row, &rows, 1),
            KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => *row = row.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => *row = (*row + 1).min(rows.len() - 1),
            KeyCode::Char('?') => self.open_modal(Modal::Help),
            KeyCode::Char('a') => self.start_pick(set, None),
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter
                if matches!(current, EdRow::Layout | EdRow::Window) =>
            {
                let d: isize = if key.code == KeyCode::Left { -1 } else { 1 };
                let s = &mut self.cfg.sets[set];
                match current {
                    EdRow::Layout => s.layout = cycle(&SetLayout::ALL, s.layout, d),
                    _ => s.window = cycle(&SetWindow::ALL, s.window, d),
                }
                self.save_config();
            }
            KeyCode::Enter => match current {
                EdRow::Name => *row = 1,
                EdRow::Entry(i) => self.edit_entry(set, i),
                EdRow::Add => self.start_pick(set, None),
                EdRow::Open => self.launch_set(set),
                _ => {}
            },
            KeyCode::Delete | KeyCode::Char('d') => {
                if let EdRow::Entry(i) = current {
                    let title = entry_label(&self.cfg.sets[set].entries[i], &self.catalog);
                    self.open_modal(Modal::Confirm(ConfirmModal {
                        message: format!("¿Quitar «{title}» del set?"),
                        detail: "La sesión no se borra.".into(),
                        action: ConfirmAction::RemoveEntry(set, i),
                        yes: false,
                    }));
                }
            }
            _ => {}
        }
        true
    }

    fn commit_set_name(&mut self, set: usize, name: &TextInput) {
        let n = name.value.trim();
        if n.is_empty() {
            self.toast(ToastKind::Warn, "El set necesita un nombre");
            return;
        }
        if self.cfg.sets[set].name != n {
            self.cfg.sets[set].name = n.to_string();
            self.save_config();
        }
    }

    fn move_entry(&mut self, set: usize, row: &mut usize, rows: &[EdRow], d: isize) {
        let EdRow::Entry(i) = rows[*row] else { return };
        let n = self.cfg.sets[set].entries.len() as isize;
        let j = i as isize + d;
        if j < 0 || j >= n {
            return;
        }
        self.cfg.sets[set].entries.swap(i, j as usize);
        *row = (*row as isize + d) as usize;
        self.save_config();
    }

    fn start_pick(&mut self, set: usize, replace: Option<usize>) {
        let mut list = ListState::default();
        let keys: Vec<String> = self.catalog.repos.iter().map(|r| r.key.clone()).collect();
        list.select(0, &keys);
        self.push(Screen::PickRepo { list, filter: Filter::default(), ctx: PickCtx { set, replace } });
    }

    fn new_set(&mut self) {
        if self.cfg_error.is_some() {
            self.toast(ToastKind::Warn, "config.toml inválido: corrígelo antes de crear sets");
            return;
        }
        let n = self.cfg.sets.len() + 1;
        self.cfg.sets.push(SessionSet { name: format!("Set {n}"), ..Default::default() });
        let i = self.cfg.sets.len() - 1;
        self.save_config();
        self.home.focus = Panel::Sets;
        let keys = self.panel_keys(Panel::Sets, "");
        self.home.sets.select(i, &keys);
        self.push(Screen::SetEditor { set: i, row: 0, name: TextInput::new(&self.cfg.sets[i].name) });
    }

    fn edit_entry(&mut self, set: usize, i: usize) {
        let e = self.cfg.sets[set].entries[i].clone();
        let repo_name = self.catalog.repo_for_path(&paths::expand(&e.repo)).map(|r| r.name.clone()).unwrap_or_else(|| folder_name(&e.repo));
        let harness = self.entry_harness(&e);
        let default_title = default_entry_title(&repo_name, &e.session, harness, &self.catalog);
        let ctx = PickCtx { set, replace: Some(i) };
        let modal = self.build_entry_modal(ctx, e.repo.clone(), e.cwd.clone(), e.session.clone(), harness, default_title, Some(&e));
        self.open_modal(Modal::Options(modal));
    }

    /// Tras elegir sesión para un set: abre el modal de la entrada.
    fn entry_options(&mut self, ctx: PickCtx, repo_key: &str, row: SessRow) {
        let Some(repo) = self.catalog.repo(repo_key) else { return };
        let repo_root = repo.root.to_string_lossy().into_owned();
        let repo_name = repo.name.clone();
        // Nueva y «última» usan el agente elegido con ⇥ (se puede cambiar en el modal).
        let (session, cwd, harness) = match row {
            SessRow::New => (SessionRef::New, None, self.new_session_harness()),
            SessRow::Latest => (SessionRef::Latest, None, self.new_session_harness()),
            SessRow::Session(i) => {
                let s = &self.catalog.sessions[i];
                let cwd = (paths::key(&s.launch_cwd) != repo.key).then(|| s.launch_cwd.to_string_lossy().into_owned());
                (SessionRef::Id(s.meta.id.clone()), cwd, s.meta.harness)
            }
        };
        let default_title = default_entry_title(&repo_name, &session, harness, &self.catalog);
        let modal = self.build_entry_modal(ctx, repo_root, cwd, session, harness, default_title, None);
        self.open_modal(Modal::Options(modal));
    }

    #[allow(clippy::too_many_arguments)]
    fn build_entry_modal(
        &self,
        ctx: PickCtx,
        repo: String,
        cwd: Option<String>,
        session: SessionRef,
        harness: Harness,
        default_title: String,
        existing: Option<&SetEntry>,
    ) -> OptionsModal {
        let o = existing.map(|e| e.options.clone()).unwrap_or_default();
        let mut fields = vec![Field::text(FieldId::Title, "Título", existing.and_then(|e| e.title.as_deref()).unwrap_or(""), &default_title)];
        if cfg!(windows) {
            fields.push(shell_field(o.shell.unwrap_or(self.default_shell())));
        }
        // El agente se elige para sesiones nuevas y «última»; una sesión concreta ya tiene el suyo.
        if !matches!(session, SessionRef::Id(_))
            && let Some(f) = self.harness_field(harness) {
                fields.push(f);
            }
        fields.extend(harness_fields(harness, session != SessionRef::New, &o));
        fields.push(color_field(&existing.and_then(|e| e.tab_color.clone()).unwrap_or_else(|| self.tab_color(harness))));
        let what = match &session {
            SessionRef::New => "Nueva sesión".to_string(),
            SessionRef::Latest => "Última sesión (al abrir)".to_string(),
            SessionRef::Id(id) => self.catalog.session(id).map(|s| s.meta.title().to_string()).unwrap_or_else(|| id.clone()),
        };
        OptionsModal {
            heading: format!("{} · {}", if ctx.replace.is_some() { "Editar entrada" } else { "Añadir al set" }, util::truncate(&what, 36)),
            sub: paths::display(Path::new(cwd.as_deref().unwrap_or(&repo))),
            purpose: OptPurpose::Entry { ctx, repo, cwd, session },
            fields,
            focus: 0,
            harness,
        }
    }

    /// Campo «Agente»: los harness que se pueden lanzar (y el actual, aunque
    /// falte). `None` si no hay nada que elegir.
    fn harness_field(&self, current: Harness) -> Option<Field> {
        let mut all = self.installed();
        if !all.contains(&current) {
            all.push(current);
            all.sort();
        }
        let opts = all.iter().map(|h| (h.id().to_string(), h.label().to_string())).collect();
        (all.len() > 1).then(|| Field::cycle(FieldId::Harness, "Agente", opts, current.id()))
    }

    /// Cambia el harness del modal: rehace los campos que dependen de él y
    /// conserva el resto (destino, shell, título, nombre…).
    fn set_modal_harness(&self, m: &mut OptionsModal, h: Harness) {
        let old_color = self.tab_color(m.harness);
        m.harness = h;
        let existing = match &m.purpose {
            OptPurpose::Launch { existing, .. } => existing.is_some(),
            OptPurpose::Entry { session, .. } => *session != SessionRef::New,
        };
        let old: Vec<Field> = std::mem::take(&mut m.fields);
        let typed: HashMap<FieldId, String> =
            old.iter().filter(|f| matches!(f.id, FieldId::Name | FieldId::Worktree)).map(|f| (f.id, f.value())).collect();
        let mut fresh = harness_fields(h, existing, &LaunchOptions::default());
        for f in &mut fresh {
            if let (Some(v), FieldKind::Text { input, .. }) = (typed.get(&f.id), &mut f.kind) {
                *input = TextInput::new(v);
            }
        }
        let mut fresh = Some(fresh);
        for f in old {
            if f.id.depends_on_harness() {
                continue;
            }
            let is_agent = f.id == FieldId::Harness;
            // El color sigue al harness mientras no se haya elegido otro.
            let f = if f.id == FieldId::Color && f.value().eq_ignore_ascii_case(&old_color) { color_field(&self.tab_color(h)) } else { f };
            m.fields.push(f);
            if is_agent && let Some(fresh) = fresh.take() {
                m.fields.extend(fresh);
            }
        }
        // El título por defecto de una entrada nombra al agente: también cambia.
        if let OptPurpose::Entry { repo, session, .. } = &m.purpose {
            let repo_name = self.catalog.repo_for_path(&paths::expand(repo)).map(|r| r.name.clone()).unwrap_or_else(|| folder_name(repo));
            let default_title = default_entry_title(&repo_name, session, h, &self.catalog);
            if let Some(Field { kind: FieldKind::Text { placeholder, .. }, .. }) = m.fields.iter_mut().find(|f| f.id == FieldId::Title) {
                *placeholder = default_title;
            }
        }
        m.focus = m.fields.iter().position(|f| f.id == FieldId::Harness).unwrap_or(0);
    }

    fn launch_fields(&self, harness: Harness, existing: bool) -> Vec<Field> {
        let g = &self.cfg.general;
        let target = match g.default_target {
            Target::Tab => "tab",
            Target::Window => "window",
            Target::Here => "here",
        };
        let mut f = vec![Field::cycle(
            FieldId::Target,
            "Destino",
            Target::ALL
                .iter()
                .map(|t| {
                    let v = match t {
                        Target::Tab => "tab",
                        Target::Window => "window",
                        Target::Here => "here",
                    };
                    (v.to_string(), t.label().to_string())
                })
                .collect(),
            target,
        )];
        // La shell solo se elige en Windows; en Linux la sesión usa `$SHELL`.
        if cfg!(windows) {
            f.push(shell_field(g.default_shell));
        }
        // El agente solo se elige al crear: una sesión existente ya tiene el suyo.
        if !existing
            && let Some(a) = self.harness_field(harness) {
                f.push(a);
            }
        f.extend(harness_fields(harness, existing, &LaunchOptions::default()));
        f
    }

    fn options_for_session(&mut self, i: usize) {
        let s = &self.catalog.sessions[i];
        let label = self.catalog.repo(&s.repo_key).map(|r| r.name.clone()).unwrap_or_default();
        let harness = s.meta.harness;
        let modal = OptionsModal {
            heading: format!("Abrir · {}", util::truncate(s.meta.title(), 40)),
            sub: paths::display(&s.launch_cwd),
            purpose: OptPurpose::Launch {
                cwd: s.launch_cwd.clone(),
                label,
                title: s.meta.title().to_string(),
                existing: Some(s.meta.id.clone()),
                file: Some(s.meta.file.clone()),
            },
            fields: self.launch_fields(harness, true),
            focus: 0,
            harness,
        };
        self.open_modal(Modal::Options(modal));
    }

    fn options_for_new(&mut self, repo_key: &str) {
        let Some(r) = self.catalog.repo(repo_key) else { return };
        let harness = self.new_session_harness();
        let modal = OptionsModal {
            heading: format!("Nueva sesión · {}", util::truncate(&r.name, 40)),
            sub: paths::display(&r.root),
            purpose: OptPurpose::Launch {
                cwd: r.root.clone(),
                label: r.name.clone(),
                title: new_session_title(&r.name, harness),
                existing: None,
                file: None,
            },
            fields: self.launch_fields(harness, false),
            focus: 0,
            harness,
        };
        self.open_modal(Modal::Options(modal));
    }

    // ------------------------------------------------------------ modales

    fn modal_key(&mut self, modal: Modal, key: KeyEvent) {
        match modal {
            Modal::Help => {}
            Modal::Options(m) => self.options_key(m, key),
            Modal::Guard(m) => self.guard_key(m, key),
            Modal::Confirm(m) => self.confirm_key(m, key),
        }
    }

    fn options_key(&mut self, mut m: OptionsModal, key: KeyEvent) {
        let n = m.fields.len();
        let is_text = matches!(m.fields[m.focus].kind, FieldKind::Text { .. });
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Enter => {
                self.submit_options(m);
                return;
            }
            KeyCode::Up | KeyCode::BackTab => m.focus = (m.focus + n - 1) % n,
            KeyCode::Down | KeyCode::Tab => m.focus = (m.focus + 1) % n,
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if !is_text => {
                if let FieldKind::Cycle { opts, idx } = &mut m.fields[m.focus].kind {
                    let len = opts.len();
                    *idx = if key.code == KeyCode::Left { (*idx + len - 1) % len } else { (*idx + 1) % len };
                }
                // Otro agente: otros modelos, permisos y opciones.
                if m.fields[m.focus].id == FieldId::Harness
                    && let Some(h) = Harness::parse(&m.fields[m.focus].value()) {
                        self.set_modal_harness(&mut m, h);
                    }
            }
            _ => {
                if let FieldKind::Text { input, .. } = &mut m.fields[m.focus].kind {
                    input.handle(&key);
                }
            }
        }
        self.modal = Some(Modal::Options(m));
    }

    fn submit_options(&mut self, m: OptionsModal) {
        let opts = m.options();
        let target = m.target();
        let entry_title = m.field(FieldId::Title).map(Field::value).filter(|t| !t.is_empty());
        let entry_color = m.field(FieldId::Color).map(Field::value);
        let harness = m.harness;
        match m.purpose {
            OptPurpose::Launch { cwd, label, title, existing, file } => {
                let mode = match existing {
                    Some(id) if opts.fork => Mode::Fork(id),
                    Some(id) => Mode::Resume(id),
                    None => Mode::New,
                };
                let title = match (&mode, opts.name.as_deref()) {
                    (Mode::New, Some(n)) => format!("{label} · {n}"),
                    // El agente pudo cambiar en el modal: el título lo sigue.
                    (Mode::New, None) => new_session_title(&label, harness),
                    (Mode::Fork(_), _) => format!("{title} (fork)"),
                    _ => title,
                };
                let planned = Planned {
                    cwd,
                    label,
                    title,
                    harness,
                    file,
                    shell: opts.shell.unwrap_or(self.default_shell()),
                    tab_color: Some(self.tab_color(harness)),
                    fixed_title: false,
                    mode,
                    opts: LaunchOptions { shell: None, ..opts },
                };
                self.request(LaunchRequest {
                    label: planned.title.clone(),
                    placement: if target == Target::Window { Placement::NewWindow } else { Placement::NewTab },
                    here: target == Target::Here,
                    items: vec![planned],
                });
            }
            OptPurpose::Entry { ctx, repo, cwd, session } => {
                let title = entry_title;
                // El color por defecto del harness no se guarda: sigue a la config.
                let color = entry_color.filter(|c| !c.eq_ignore_ascii_case(&self.tab_color(harness)));
                let mut options = opts;
                if options.shell == Some(self.default_shell()) {
                    options.shell = None;
                }
                let entry = SetEntry { repo, cwd, session, harness, title, tab_color: color, options };
                let Some(set) = self.cfg.sets.get_mut(ctx.set) else { return };
                match ctx.replace {
                    Some(i) if i < set.entries.len() => set.entries[i] = entry,
                    _ => set.entries.push(entry),
                }
                let saved = self.save_config();
                // Volver al editor del set.
                while matches!(self.stack.last(), Some(Screen::Repo { pick: Some(_), .. }) | Some(Screen::PickRepo { .. })) {
                    self.stack.pop();
                }
                self.pop();
                if let Some(Screen::SetEditor { row, .. }) = self.stack.last_mut() {
                    let n = self.cfg.sets[ctx.set].entries.len();
                    *row = 3 + ctx.replace.unwrap_or(n.saturating_sub(1));
                }
                if saved {
                    self.toast(ToastKind::Success, if ctx.replace.is_some() { "Entrada actualizada" } else { "Sesión añadida al set" });
                }
            }
        }
    }

    fn guard_key(&mut self, mut m: GuardModal, key: KeyEvent) {
        let choices = m.choices();
        let choice = match key.code {
            KeyCode::Esc | KeyCode::Char('n') => return,
            KeyCode::Char('f') if m.can_fork => GuardChoice::Fork,
            KeyCode::Char('a') => GuardChoice::OpenAnyway,
            KeyCode::Up => {
                m.choice = m.choice.saturating_sub(1);
                self.modal = Some(Modal::Guard(m));
                return;
            }
            KeyCode::Down | KeyCode::Tab => {
                m.choice = (m.choice + 1).min(choices.len() - 1);
                self.modal = Some(Modal::Guard(m));
                return;
            }
            KeyCode::Enter => choices[m.choice.min(choices.len() - 1)],
            _ => {
                self.modal = Some(Modal::Guard(m));
                return;
            }
        };
        match choice {
            GuardChoice::Fork => {
                for c in &m.conflicts {
                    let p = &mut m.request.items[c.item];
                    if let Mode::Resume(id) = &p.mode {
                        p.mode = Mode::Fork(id.clone());
                        p.title = format!("{} (fork)", p.title);
                    }
                }
                self.execute(m.request);
            }
            GuardChoice::OpenAnyway => self.execute(m.request),
            GuardChoice::Cancel => {}
        }
    }

    fn confirm_key(&mut self, mut m: ConfirmModal, key: KeyEvent) {
        let yes = match key.code {
            KeyCode::Char('s') | KeyCode::Char('y') => true,
            KeyCode::Char('n') | KeyCode::Esc => return,
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                m.yes = !m.yes;
                self.modal = Some(Modal::Confirm(m));
                return;
            }
            KeyCode::Enter => m.yes,
            _ => {
                self.modal = Some(Modal::Confirm(m));
                return;
            }
        };
        if !yes {
            return;
        }
        match m.action {
            ConfirmAction::DeleteSet(i) if i < self.cfg.sets.len() => {
                let name = self.cfg.sets.remove(i).name;
                if self.save_config() {
                    self.toast(ToastKind::Success, format!("Set «{name}» borrado"));
                }
                // Cierra editores que apunten a índices desplazados.
                self.stack.retain(|s| !matches!(s, Screen::SetEditor { .. } | Screen::PickRepo { .. } | Screen::Repo { pick: Some(_), .. }));
                self.sync_lists();
            }
            ConfirmAction::RemoveEntry(s, i) if s < self.cfg.sets.len() && i < self.cfg.sets[s].entries.len() => {
                self.cfg.sets[s].entries.remove(i);
                self.save_config();
            }
            ConfirmAction::Bypass(on) => self.apply_bypass(on),
            _ => {}
        }
    }

    // ------------------------------------------------------------ lanzar

    fn planned_for_session(&self, i: usize, opts: LaunchOptions) -> Planned {
        let s = &self.catalog.sessions[i];
        let label = self.catalog.repo(&s.repo_key).map(|r| r.name.clone()).unwrap_or_default();
        let harness = s.meta.harness;
        Planned {
            cwd: s.launch_cwd.clone(),
            label,
            title: s.meta.title().to_string(),
            harness,
            file: Some(s.meta.file.clone()),
            shell: opts.shell.unwrap_or(self.default_shell()),
            tab_color: Some(self.tab_color(harness)),
            fixed_title: false,
            mode: if opts.fork && harness.info().fork { Mode::Fork(s.meta.id.clone()) } else { Mode::Resume(s.meta.id.clone()) },
            opts,
        }
    }

    fn single_placement(&self) -> (Placement, bool) {
        match self.cfg.general.default_target {
            Target::Tab => (Placement::NewTab, false),
            Target::Window => (Placement::NewWindow, false),
            Target::Here => (Placement::NewTab, true),
        }
    }

    pub fn launch_session(&mut self, i: usize) {
        let s = &self.catalog.sessions[i];
        if !s.cwd_exists {
            self.toast(ToastKind::Error, format!("La carpeta {} ya no existe", paths::display(&s.launch_cwd)));
            return;
        }
        let p = self.planned_for_session(i, LaunchOptions::default());
        let (placement, here) = self.single_placement();
        self.request(LaunchRequest { label: p.title.clone(), items: vec![p], placement, here });
    }

    pub fn launch_new(&mut self, repo_key: &str) {
        let Some(r) = self.catalog.repo(repo_key) else { return };
        if !r.exists {
            self.toast(ToastKind::Error, format!("La carpeta {} ya no existe", paths::display(&r.root)));
            return;
        }
        let harness = self.new_session_harness();
        let p = Planned {
            cwd: r.root.clone(),
            label: r.name.clone(),
            title: new_session_title(&r.name, harness),
            harness,
            file: None,
            shell: self.default_shell(),
            tab_color: Some(self.tab_color(harness)),
            fixed_title: false,
            mode: Mode::New,
            opts: LaunchOptions::default(),
        };
        let (placement, here) = self.single_placement();
        self.request(LaunchRequest { label: p.title.clone(), items: vec![p], placement, here });
    }

    pub fn launch_set(&mut self, set: usize) {
        let Some(s) = self.cfg.sets.get(set).cloned() else { return };
        if s.entries.is_empty() {
            self.toast(ToastKind::Warn, format!("El set «{}» no tiene sesiones: añade con a", s.name));
            return;
        }
        let mut items = Vec::new();
        let mut skipped = Vec::new();
        for e in &s.entries {
            let dir = e.launch_dir();
            let repo_name = self.catalog.repo_for_path(&dir).map(|r| r.name.clone()).unwrap_or_else(|| folder_name(&e.repo));
            if !dir.is_dir() {
                skipped.push(format!("{repo_name} (carpeta no existe)"));
                continue;
            }
            // Sesión que se retoma (y su harness, que manda sobre el de la entrada).
            let found = match &e.session {
                SessionRef::New => None,
                // La más reciente de ese harness en la carpeta; si no hay, una nueva.
                SessionRef::Latest => {
                    let k = paths::key(&dir);
                    self.catalog.sessions.iter().find(|x| x.meta.harness == e.harness && paths::key(&x.launch_cwd) == k)
                }
                SessionRef::Id(id) => match self.catalog.session(id) {
                    Some(x) => Some(x),
                    None => {
                        let short: String = id.chars().take(8).collect();
                        skipped.push(format!("{repo_name} (sesión {short} no encontrada)"));
                        continue;
                    }
                },
            };
            let harness = found.map_or(e.harness, |x| x.meta.harness);
            let mode = match found {
                Some(x) if e.options.fork && harness.info().fork => Mode::Fork(x.meta.id.clone()),
                Some(x) => Mode::Resume(x.meta.id.clone()),
                None => Mode::New,
            };
            let title = e.title.clone().unwrap_or_else(|| default_entry_title(&repo_name, &e.session, harness, &self.catalog));
            items.push(Planned {
                cwd: dir,
                label: repo_name,
                harness,
                file: found.map(|x| x.meta.file.clone()),
                fixed_title: e.title.is_some(),
                title,
                shell: e.options.shell.unwrap_or(self.default_shell()),
                tab_color: Some(e.tab_color.clone().unwrap_or_else(|| self.tab_color(harness))),
                mode,
                opts: e.options.clone(),
            });
        }
        if !skipped.is_empty() {
            self.toast(ToastKind::Warn, format!("Omitidas: {}", skipped.join(", ")));
        }
        if items.is_empty() {
            return;
        }
        self.request(LaunchRequest { label: s.name.clone(), items, placement: Placement::Set(s.layout, s.window), here: false });
    }

    /// Revisa conflictos (sesión ya abierta) y lanza o abre la guarda.
    fn request(&mut self, req: LaunchRequest) {
        let now = self.now_ms();
        let mut conflicts = Vec::new();
        for (i, p) in req.items.iter().enumerate() {
            let Mode::Resume(id) = &p.mode else { continue };
            if let Some(l) = self.live.get(id) {
                let (surface, _) = super::theme::entrypoint(&l.entrypoint);
                let state = if l.is_busy() {
                    " · trabajando"
                } else if l.is_waiting() {
                    " · esperando respuesta"
                } else if l.status == "idle" {
                    " · inactiva"
                } else {
                    // Solo se sabe que el proceso sigue vivo.
                    ""
                };
                let since = util::relative_time(l.started_ms, now);
                conflicts.push(Conflict {
                    item: i,
                    title: p.title.clone(),
                    detail: format!("abierta en {} · pid {}{state} · desde {since}", if surface.is_empty() { "otra superficie" } else { surface }, l.pid),
                });
            } else if let Some(s) = self.catalog.session(id)
                && now - s.meta.mtime_ms < RECENT_ACTIVITY_MS {
                    conflicts.push(Conflict {
                        item: i,
                        title: p.title.clone(),
                        detail: format!("con actividad {} (puede seguir abierta)", util::relative_time(s.meta.mtime_ms, now)),
                    });
                }
        }
        if conflicts.is_empty() {
            self.execute(req);
        } else {
            // El fork solo se ofrece si todos los harness en conflicto saben bifurcar.
            let can_fork = conflicts.iter().all(|c| req.items[c.item].harness.info().fork);
            self.open_modal(Modal::Guard(GuardModal { request: req, conflicts, choice: 0, can_fork }));
        }
    }

    fn execute(&mut self, req: LaunchRequest) {
        let items: Vec<LaunchItem> = req
            .items
            .iter()
            .map(|p| {
                let (args, session_id) = p.harness.args(&p.mode, &p.opts, p.file.as_deref());
                LaunchItem {
                    cwd: p.harness.launch_cwd(&p.cwd),
                    harness: p.harness,
                    args,
                    title: p.title.clone(),
                    label: p.label.clone(),
                    shell: p.shell,
                    tab_color: p.tab_color.clone(),
                    fixed_title: p.fixed_title,
                    session_id,
                }
            })
            .collect();
        if let Some(log) = &mut self.dry_run {
            log.push((items, req.placement, req.here));
            if self.demo {
                self.toast(ToastKind::Info, format!("Modo demo: aquí se abriría {}", req.label));
            }
            return;
        }
        let env = match &self.env {
            Ok(e) => e.clone(),
            Err(e) => {
                self.toast(ToastKind::Error, e.clone());
                return;
            }
        };
        // Un harness sin instalar se avisa antes de abrir nada.
        if let Some(e) = items.iter().find_map(|it| env.command(it.harness).err()) {
            self.toast(ToastKind::Error, format!("No se pudo abrir: {e:#}"));
            return;
        }
        self.fx.flash = true;
        if req.here {
            self.run_here = items.into_iter().next();
            return;
        }
        match env.launch(&items, req.placement) {
            Ok(tokens) => {
                let n = tokens.len();
                self.batches.push(Batch {
                    label: req.label.clone(),
                    tokens: tokens.into_iter().map(|(t, _)| t).collect(),
                    confirmed: 0,
                    born: Instant::now(),
                });
                let what = if n == 1 { "Abriendo".to_string() } else { format!("Abriendo {n} sesiones de") };
                self.toast(ToastKind::Info, format!("{what} {}…", req.label));
            }
            Err(e) => self.toast(ToastKind::Error, format!("No se pudo abrir: {e:#}")),
        }
    }

    // ------------------------------------------------------------ ratón

    pub fn on_mouse(&mut self, ev: MouseEvent) {
        if self.modal.is_some() {
            if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
                // Click fuera del modal no hace nada; dentro, tampoco (teclado).
            }
            return;
        }
        let (x, y) = (ev.column, ev.row);
        let Some(hit) = self.hits.iter().find(|h| x >= h.rect.x && x < h.rect.right() && y >= h.rect.y && y < h.rect.bottom()).cloned()
        else {
            return;
        };
        match ev.kind {
            MouseEventKind::ScrollDown => self.mouse_step(hit.list, 1),
            MouseEventKind::ScrollUp => self.mouse_step(hit.list, -1),
            MouseEventKind::Down(MouseButton::Left) => {
                let idx = match hit.idx {
                    Some(i) => i,
                    None => hit.offset + ((y - hit.rect.y) / hit.item_h.max(1)) as usize,
                };
                if idx >= hit.count {
                    return;
                }
                let now = Instant::now();
                let double = matches!(self.last_click, Some((t, l, i)) if l == hit.list && i == idx && now.duration_since(t) < Duration::from_millis(450));
                self.last_click = Some((now, hit.list, idx));
                self.mouse_select(hit.list, idx);
                if double {
                    self.last_click = None;
                    self.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                }
            }
            _ => {}
        }
    }

    fn mouse_step(&mut self, list: ListId, d: isize) {
        let code = if d > 0 { KeyCode::Down } else { KeyCode::Up };
        if let ListId::Panel(p) = list {
            self.focus_panel(p);
        }
        if list == ListId::SetRows {
            return;
        }
        self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn mouse_select(&mut self, list: ListId, idx: usize) {
        match list {
            ListId::Panel(p) => {
                self.focus_panel(p);
                let keys = self.panel_keys(p, self.home.filter.text());
                self.home_list(p).select(idx, &keys);
            }
            _ => {
                let mut stack = std::mem::take(&mut self.stack);
                match stack.last_mut() {
                    Some(Screen::Repo { key, list: l, filter, pick }) => {
                        let rows = self.session_rows(key, filter.text(), pick.is_some());
                        l.select(idx, &self.sess_keys(&rows));
                    }
                    Some(Screen::PickRepo { list: l, filter, .. }) => {
                        let keys: Vec<String> = self.repo_rows(filter.text()).into_iter().map(|i| self.catalog.repos[i].key.clone()).collect();
                        l.select(idx, &keys);
                    }
                    Some(Screen::Map { window, .. }) => {
                        if list == ListId::MapNodes {
                            let w = *window;
                            self.stack = stack;
                            let g = self.graph(w);
                            stack = std::mem::take(&mut self.stack);
                            if let (Some(node), Some(Screen::Map { sel, .. })) = (g.nodes.get(idx), stack.last_mut()) {
                                *sel = Some(node.key.clone());
                            }
                        }
                    }
                    Some(Screen::SetEditor { row, set, name }) => {
                        if *row == 0 && idx != 0 {
                            let n = name.clone();
                            let s = *set;
                            self.stack = stack;
                            self.commit_set_name(s, &n);
                            stack = std::mem::take(&mut self.stack);
                            if let Some(Screen::SetEditor { row, .. }) = stack.last_mut() {
                                *row = idx;
                            }
                        } else {
                            *row = idx;
                        }
                    }
                    None => {}
                }
                self.stack = stack;
            }
        }
    }
}

impl App {
    /// Prepara un estado para `claunch snapshot` (solo desarrollo; no guarda nada
    /// ni lanza sesiones).
    pub fn apply_snapshot_script(&mut self, screen: &str) {
        self.read_only = true;
        let first_repo = self.catalog.repos.iter().position(|r| r.sessions.len() > 2).unwrap_or(0);
        let repo_key = self.catalog.repos.get(first_repo).map(|r| r.key.clone()).unwrap_or_default();
        match screen {
            "recents" => self.home.focus = Panel::Recents,
            "sets" | "editor" | "editor-panes" | "editor-windows" => {
                if self.cfg.sets.is_empty() {
                    let entries = self
                        .catalog
                        .repos
                        .iter()
                        .take(3)
                        .enumerate()
                        .map(|(i, r)| SetEntry {
                            repo: r.root.to_string_lossy().into_owned(),
                            cwd: None,
                            session: if i == 1 { SessionRef::New } else { SessionRef::Latest },
                            harness: Harness::Claude,
                            title: None,
                            tab_color: Some(super::theme::TAB_COLORS[i].0.into()),
                            options: LaunchOptions::default(),
                        })
                        .collect();
                    self.cfg.sets.push(SessionSet { name: "Mi proyecto".into(), layout: SetLayout::Panes, entries, ..Default::default() });
                }
                self.home.focus = Panel::Sets;
                if screen.starts_with("editor") {
                    if screen == "editor-windows" {
                        self.cfg.sets[0].layout = SetLayout::Windows;
                    } else if screen == "editor" {
                        self.cfg.sets[0].layout = SetLayout::Tabs;
                    }
                    self.stack.push(Screen::SetEditor { set: 0, row: 3, name: TextInput::new(&self.cfg.sets[0].name) });
                }
            }
            "help" => self.modal = Some(Modal::Help),
            "map" => self.stack.push(Screen::Map { sel: None, window: 1 }),
            "map-week" => self.stack.push(Screen::Map { sel: None, window: 2 }),
            "bypass" => self.ask_bypass(),
            "repo" | "options" | "new" | "guard" => {
                self.open_repo(repo_key.clone(), None);
                if let Some(Screen::Repo { list, .. }) = self.stack.last_mut() {
                    list.idx = 1;
                    list.key = None;
                }
                let sess = self.catalog.repo(&repo_key).and_then(|r| r.sessions.first().copied());
                match (screen, sess) {
                    ("options", Some(i)) => self.options_for_session(i),
                    ("new", _) => self.options_for_new(&repo_key),
                    ("guard", Some(i)) => {
                        let p = self.planned_for_session(i, LaunchOptions::default());
                        let title = p.title.clone();
                        let can_fork = p.harness.info().fork;
                        self.modal = Some(Modal::Guard(GuardModal {
                            request: LaunchRequest { label: title.clone(), items: vec![p], placement: Placement::NewTab, here: false },
                            conflicts: vec![Conflict { item: 0, title, detail: "abierta en VS Code · pid 2372 · inactiva · desde hace 2 h".into() }],
                            choice: 0,
                            can_fork,
                        }));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        self.fx.screen = None;
        self.fx.modal = false;
    }
}

// ---------------------------------------------------------------- helpers

fn cycle<T: Copy + PartialEq>(all: &[T], cur: T, d: isize) -> T {
    let i = all.iter().position(|x| *x == cur).unwrap_or(0) as isize;
    all[((i + d).rem_euclid(all.len() as isize)) as usize]
}

fn shell_field(current: Shell) -> Field {
    let v = |s: Shell| match s {
        Shell::GitBash => "git-bash",
        Shell::Powershell => "powershell",
        Shell::Cmd => "cmd",
    };
    Field::cycle(FieldId::Shell, "Shell", Shell::ALL.iter().map(|s| (v(*s).to_string(), s.label().to_string())).collect(), v(current))
}

fn mode_field(fork: bool) -> Field {
    Field::cycle(
        FieldId::Mode,
        "Modo",
        vec![("resume".into(), "Reanudar".into()), ("fork".into(), "Fork".into())],
        if fork { "fork" } else { "resume" },
    )
}

/// Campos que dependen del harness: modo (reanudar o fork), modelo, permisos
/// y, para sesiones nuevas, nombre y worktree. Solo los que el harness admite.
fn harness_fields(h: Harness, existing: bool, o: &LaunchOptions) -> Vec<Field> {
    let info = h.info();
    let mut f = Vec::new();
    if existing && info.fork {
        f.push(mode_field(o.fork));
    }
    f.extend(model_field(h, o.model.as_deref().unwrap_or("")));
    f.extend(permission_field(h, o.permission_mode.as_deref().unwrap_or("")));
    if !existing {
        if info.name {
            f.push(Field::text(FieldId::Name, "Nombre", o.name.as_deref().unwrap_or(""), "opcional"));
        }
        if info.worktree {
            f.push(Field::text(FieldId::Worktree, "Worktree", o.worktree.as_deref().unwrap_or(""), "opcional"));
        }
    }
    f
}

/// Campo de modelo: lista cerrada, texto libre o nada, según el harness.
fn model_field(h: Harness, current: &str) -> Option<Field> {
    match h.info().model {
        ModelInput::Choice(models) => {
            let mut opts: Vec<(String, String)> =
                models.iter().map(|m| (m.to_string(), if m.is_empty() { "por defecto".into() } else { m.to_string() })).collect();
            // Un modelo escrito a mano en la config no se pierde al editar.
            if !models.contains(&current) {
                opts.push((current.to_string(), current.to_string()));
            }
            Some(Field::cycle(FieldId::Model, "Modelo", opts, current))
        }
        ModelInput::Free(example) => Some(Field::text(FieldId::Model, "Modelo", current, &format!("por defecto · p. ej. {example}"))),
        ModelInput::None => None,
    }
}

/// Campo de permisos con los modos del harness; `None` si solo tiene el de por defecto.
fn permission_field(h: Harness, current: &str) -> Option<Field> {
    let perms = h.info().permissions;
    if perms.len() < 2 {
        return None;
    }
    let mut opts: Vec<(String, String)> = perms.iter().map(|p| (p.value.to_string(), p.label.to_string())).collect();
    let mut hints: Vec<&'static str> = perms.iter().map(|p| p.hint).collect();
    if !perms.iter().any(|p| p.value == current) {
        opts.push((current.to_string(), current.to_string()));
        hints.push("");
    }
    let mut field = Field::cycle(FieldId::Permission, "Permisos", opts, current);
    field.hints = hints;
    Some(field)
}

/// Campo de color de pestaña: la paleta y, si no está en ella, el color actual.
fn color_field(color: &str) -> Field {
    let mut opts: Vec<(String, String)> = super::theme::TAB_COLORS.iter().map(|(h, n)| (h.to_string(), n.to_string())).collect();
    if !opts.iter().any(|(h, _)| h.eq_ignore_ascii_case(color)) {
        opts.insert(0, (color.to_string(), color.to_string()));
    }
    let mut field = Field::cycle(FieldId::Color, "Color", opts, "");
    if let FieldKind::Cycle { opts, idx } = &mut field.kind {
        *idx = opts.iter().position(|(h, _)| h.eq_ignore_ascii_case(color)).unwrap_or(0);
    }
    field
}

pub fn folder_name(p: &str) -> String {
    Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string())
}

/// «repo» para Claude y «repo · codex» para los demás: así dos pestañas del
/// mismo repo con agentes distintos no se llaman igual.
fn repo_and_agent(repo_name: &str, harness: Harness) -> String {
    match harness {
        Harness::Claude => repo_name.to_string(),
        h => format!("{repo_name} · {}", h.tag()),
    }
}

/// Título de pestaña de una sesión nueva.
pub fn new_session_title(repo_name: &str, harness: Harness) -> String {
    format!("{} · nueva", repo_and_agent(repo_name, harness))
}

/// Título por defecto de una entrada de set. `harness` es el de la entrada
/// (para una sesión concreta no cuenta: la nombra su propio título).
pub fn default_entry_title(repo_name: &str, session: &SessionRef, harness: Harness, catalog: &Catalog) -> String {
    match session {
        SessionRef::New => new_session_title(repo_name, harness),
        SessionRef::Latest => format!("{} · última", repo_and_agent(repo_name, harness)),
        SessionRef::Id(id) => {
            let t = catalog.session(id).map(|s| s.meta.title().to_string()).unwrap_or_else(|| id.chars().take(8).collect());
            format!("{repo_name} · {}", util::truncate(&t, 28))
        }
    }
}

pub fn entry_label(e: &SetEntry, catalog: &Catalog) -> String {
    if let Some(t) = &e.title {
        return t.clone();
    }
    let dir = e.launch_dir();
    let repo = catalog.repo_for_path(&dir).map(|r| r.name.clone()).unwrap_or_else(|| folder_name(&e.repo));
    default_entry_title(&repo, &e.session, e.harness, catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::SessionMeta;

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }
    fn ch(c: char) -> KeyEvent {
        key(KeyCode::Char(c))
    }
    fn typed(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_key(ch(c));
        }
    }

    /// Entorno de pruebas con estos agentes «instalados», sin mirar el equipo.
    fn test_env(installed: &[Harness]) -> Env {
        Env {
            commands: installed.iter().map(|h| (*h, vec![h.info().program.to_string()])).collect(),
            problems: Default::default(),
            git_bash: None,
            terminal: crate::launch::Terminal::Unavailable,
            self_exe: PathBuf::from("claunch"),
            general: crate::config::General::default(),
        }
    }

    /// Dos repos: beta (sesión s3, la más reciente) y alpha (s1, s2), con solo
    /// Claude instalado.
    fn fixture(name: &str) -> (App, PathBuf, PathBuf) {
        fixture_with(name, &[Harness::Claude], &[])
    }

    /// Como `fixture`, con estos agentes instalados y sesiones extra:
    /// (id, harness, en beta, antigüedad en ms, título).
    fn fixture_with(name: &str, installed: &[Harness], extra: &[(&str, Harness, bool, i64, &str)]) -> (App, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join("claunch-tests").join(name);
        let alpha = base.join("alpha");
        let beta = base.join("beta");
        for r in [&alpha, &beta] {
            std::fs::create_dir_all(r.join(".git")).unwrap();
        }
        let now = util::now_ms();
        let meta = |id: &str, harness: Harness, cwd: &Path, age: i64, title: &str| SessionMeta {
            harness,
            id: id.into(),
            file: base.join(format!("{id}.jsonl")),
            mtime_ms: now - age,
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ai_title: Some(title.into()),
            has_messages: true,
            prompts: 3,
            ..Default::default()
        };
        let mut metas = vec![
            meta("s1", Harness::Claude, &alpha, 3_600_000, "Primera alfa"),
            meta("s2", Harness::Claude, &alpha, 7_200_000, "Segunda alfa"),
            meta("s3", Harness::Claude, &beta, 600_000, "Beta reciente"),
        ];
        for (id, harness, in_beta, age, title) in extra {
            metas.push(meta(id, *harness, if *in_beta { &beta } else { &alpha }, *age, title));
        }
        let mut cfg = Config::default();
        cfg.general.animations = false;
        let mut app = App::new(cfg, None, None);
        app.env = Ok(test_env(installed));
        app.catalog = Arc::new(Catalog::build(&metas, &[]));
        app.loaded = true;
        app.read_only = true;
        app.dry_run = Some(Vec::new());
        app.toasts.clear();
        app.sync_lists();
        (app, paths::normalize(&alpha), paths::normalize(&beta))
    }

    /// Los cinco agentes instalados y, además de las de Claude, una sesión de
    /// Codex en beta (x1, la más reciente de todas) y una de Pi en alpha (p1).
    fn mixed(name: &str) -> (App, PathBuf, PathBuf) {
        fixture_with(
            name,
            &Harness::ALL,
            &[("x1", Harness::Codex, true, 300_000, "Codex en beta"), ("p1", Harness::Pi, false, 900_000, "Pi en alfa")],
        )
    }

    fn options(app: &App) -> &OptionsModal {
        match &app.modal {
            Some(Modal::Options(m)) => m,
            _ => panic!("se esperaba el modal de opciones"),
        }
    }

    /// El modal ofrece exactamente lo que el harness admite.
    fn assert_fields_match(m: &OptionsModal, h: Harness, existing: bool) {
        let info = h.info();
        let has = |id: FieldId| m.fields.iter().any(|f| f.id == id);
        assert_eq!(has(FieldId::Mode), existing && info.fork, "{}: modo", info.id);
        assert_eq!(has(FieldId::Model), info.model != ModelInput::None, "{}: modelo", info.id);
        assert_eq!(has(FieldId::Permission), info.permissions.len() > 1, "{}: permisos", info.id);
        assert_eq!(has(FieldId::Name), !existing && info.name, "{}: nombre", info.id);
        assert_eq!(has(FieldId::Worktree), !existing && info.worktree, "{}: worktree", info.id);
        assert!(m.preview().starts_with(info.program), "{}: {}", info.id, m.preview());
    }

    fn launched(app: &App) -> &Vec<(Vec<LaunchItem>, Placement, bool)> {
        app.dry_run.as_ref().unwrap()
    }

    #[test]
    fn repos_sorted_and_navigation() {
        let (mut app, _alpha, beta) = fixture("nav");
        assert_eq!(app.catalog.repos[0].root, beta);
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.stack.last(), Some(Screen::Repo { key, .. }) if *key == paths::key(&beta)));
        app.on_key(key(KeyCode::Esc));
        assert!(app.stack.is_empty());
    }

    #[test]
    fn new_session_is_first_and_launches_in_repo_root() {
        let (mut app, _alpha, beta) = fixture("new");
        app.on_key(key(KeyCode::Enter)); // entra a beta
        app.on_key(key(KeyCode::Enter)); // «Nueva sesión»
        let (items, placement, here) = &launched(&app)[0];
        assert_eq!(items[0].cwd, beta);
        assert_eq!(items[0].args[0], "--session-id");
        assert_eq!(*placement, Placement::NewTab);
        assert!(!here);
    }

    #[test]
    fn guard_offers_fork_for_live_session() {
        let (mut app, _a, _b) = fixture("guard");
        let mut live = HashMap::new();
        live.insert(
            "s3".to_string(),
            LiveSession { pid: 1, session_id: "s3".into(), entrypoint: "claude-vscode".into(), status: "idle".into(), ..Default::default() },
        );
        app.live = Arc::new(live);
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Guard(_))));
        assert!(launched(&app).is_empty());
        app.on_key(ch('f'));
        assert_eq!(launched(&app)[0].0[0].args, vec!["-r", "s3", "--fork-session"]);
    }

    #[test]
    fn filter_then_enter_opens_match() {
        let (mut app, alpha, _b) = fixture("filter");
        app.on_key(ch('/'));
        typed(&mut app, "alp");
        assert_eq!(app.repo_rows("alp").len(), 1);
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.stack.last(), Some(Screen::Repo { key, .. }) if *key == paths::key(&alpha)));
    }

    #[test]
    fn options_modal_fork_and_window() {
        let (mut app, _a, _b) = fixture("options");
        app.on_key(ch('2')); // Recientes
        app.on_key(ch('o'));
        assert!(matches!(app.modal, Some(Modal::Options(_))));
        app.on_key(key(KeyCode::Right)); // Destino → ventana nueva
        // Bajar hasta «Modo» (en Windows hay antes una fila «Shell»).
        let Some(Modal::Options(m)) = &app.modal else { unreachable!() };
        let mode = m.fields.iter().position(|f| f.id == FieldId::Mode).unwrap();
        for _ in 0..mode {
            app.on_key(key(KeyCode::Down));
        }
        app.on_key(key(KeyCode::Right)); // → fork
        app.on_key(key(KeyCode::Enter));
        let (items, placement, _) = &launched(&app)[0];
        assert_eq!(items[0].args, vec!["-r", "s3", "--fork-session"]);
        assert_eq!(*placement, Placement::NewWindow);
        assert!(items[0].title.ends_with("(fork)"));
    }

    #[test]
    fn build_set_end_to_end() {
        let (mut app, _a, beta) = fixture("set");
        app.on_key(ch('n'));
        assert_eq!(app.cfg.sets.len(), 1);
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        typed(&mut app, "Gestión");
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.cfg.sets[0].name, "Gestión");
        app.on_key(ch('a')); // elegir repo
        assert!(matches!(app.stack.last(), Some(Screen::PickRepo { .. })));
        app.on_key(key(KeyCode::Enter)); // beta
        app.on_key(key(KeyCode::Down)); // «Última sesión»
        app.on_key(key(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Options(_))));
        app.on_key(key(KeyCode::Enter)); // guardar entrada
        assert!(matches!(app.stack.last(), Some(Screen::SetEditor { .. })));
        assert_eq!(app.cfg.sets[0].entries.len(), 1);
        assert_eq!(app.cfg.sets[0].entries[0].session, SessionRef::Latest);
        // Bajar hasta «Abrir set» y lanzarlo.
        for _ in 0..10 {
            app.on_key(key(KeyCode::Down));
        }
        app.on_key(key(KeyCode::Enter));
        let (items, placement, _) = &launched(&app)[0];
        assert_eq!(*placement, Placement::Set(SetLayout::Tabs, SetWindow::New));
        assert_eq!(items[0].cwd, beta);
        assert_eq!(items[0].args, vec!["-r", "s3"]);
        assert_eq!(items[0].harness, Harness::Claude);
    }

    #[test]
    fn tab_picks_the_agent_for_new_sessions() {
        let (mut app, _alpha, beta) = fixture_with("tab-agent", &[Harness::Claude, Harness::Codex, Harness::Pi], &[]);
        app.on_key(key(KeyCode::Enter)); // entra a beta
        assert_eq!(app.new_session_harness(), Harness::Claude);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.new_session_harness(), Harness::Codex);
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.new_session_harness(), Harness::Claude, "da la vuelta");
        app.on_key(key(KeyCode::BackTab));
        assert_eq!(app.new_session_harness(), Harness::Pi);
        app.on_key(key(KeyCode::Enter)); // «Nueva sesión» con Pi
        let (items, _, _) = &launched(&app)[0];
        assert_eq!(items[0].harness, Harness::Pi);
        // Los harness que no son Claude arrancan con la ruta como la da el sistema.
        assert_eq!(items[0].cwd, paths::natural(&beta));
        assert_eq!(items[0].tab_color.as_deref(), Some(Harness::Pi.info().color));
        // Con otro agente que no sea Claude, el título de la pestaña lo nombra.
        assert_eq!(items[0].title, "beta · pi · nueva");
    }

    #[test]
    fn new_session_agent_falls_back_to_an_installed_one() {
        let (mut app, _a, _b) = fixture("agent-fallback");
        app.new_harness = Harness::Codex;
        assert_eq!(app.new_session_harness(), Harness::Claude);
        // Con un solo agente no hay nada que cambiar.
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.new_session_harness(), Harness::Claude);
        // Sin ninguno instalado se respeta el elegido: el fallo dirá cuál falta.
        app.env = Ok(test_env(&[]));
        assert_eq!(app.new_session_harness(), Harness::Codex);
        // Un color general propio manda sobre el de cada harness.
        assert_eq!(app.tab_color(Harness::Codex), Harness::Codex.info().color);
        app.cfg.general.tab_color = "#123456".into();
        assert_eq!(app.tab_color(Harness::Codex), "#123456");
    }

    #[test]
    fn options_modal_follows_the_chosen_agent() {
        let (mut app, _a, _b) = fixture_with("agent-field", &Harness::ALL, &[]);
        app.on_key(key(KeyCode::Enter)); // entra a beta
        app.on_key(ch('o')); // opciones de «Nueva sesión»
        assert_eq!(options(&app).harness, Harness::Claude);
        assert_fields_match(options(&app), Harness::Claude, false);
        let agent = options(&app).fields.iter().position(|f| f.id == FieldId::Harness).expect("campo Agente");
        for _ in 0..agent {
            app.on_key(key(KeyCode::Down));
        }
        for expected in [Harness::Codex, Harness::Opencode, Harness::Antigravity, Harness::Pi, Harness::Claude] {
            app.on_key(key(KeyCode::Right));
            let m = options(&app);
            assert_eq!(m.harness, expected);
            assert_eq!(m.fields[m.focus].id, FieldId::Harness, "el foco sigue en Agente");
            assert_fields_match(m, expected, false);
            // Destino y agente siguen ahí; ningún campo se repite.
            let mut ids: Vec<FieldId> = m.fields.iter().map(|f| f.id).collect();
            assert!(ids.contains(&FieldId::Target));
            let n = ids.len();
            ids.dedup();
            assert_eq!(ids.len(), n);
        }
        app.on_key(key(KeyCode::Left)); // de Claude vuelve a Pi
        app.on_key(key(KeyCode::Enter));
        let (items, _, _) = &launched(&app)[0];
        assert_eq!(items[0].harness, Harness::Pi);
        // El título sigue al agente elegido en el modal, no al que había al abrirlo.
        assert_eq!(items[0].title, "beta · pi · nueva");
    }

    #[test]
    fn entry_titles_and_colors_follow_the_agent() {
        let (mut app, _alpha, beta) = mixed("entry-agent");
        let entry = |session: SessionRef, harness: Harness| SetEntry {
            repo: beta.to_string_lossy().into_owned(),
            cwd: None,
            session,
            harness,
            title: None,
            tab_color: None,
            options: LaunchOptions::default(),
        };
        // Mismo repo, dos agentes: títulos distintos.
        assert_eq!(entry_label(&entry(SessionRef::Latest, Harness::Claude), &app.catalog), "beta · última");
        assert_eq!(entry_label(&entry(SessionRef::Latest, Harness::Codex), &app.catalog), "beta · codex · última");
        assert_eq!(entry_label(&entry(SessionRef::New, Harness::Opencode), &app.catalog), "beta · opencode · nueva");
        // Una sesión concreta manda sobre el harness de la entrada: color y agente son los suyos.
        let stale = entry(SessionRef::Id("x1".into()), Harness::Claude);
        assert_eq!(app.entry_harness(&stale), Harness::Codex);
        assert_eq!(app.entry_color(&stale), Harness::Codex.info().color);
        assert_eq!(app.entry_color(&entry(SessionRef::New, Harness::Pi)), Harness::Pi.info().color);
        // El color propio de la entrada, o uno general elegido por el usuario, mandan.
        let own = SetEntry { tab_color: Some("#000001".into()), ..entry(SessionRef::New, Harness::Pi) };
        assert_eq!(app.entry_color(&own), "#000001");
        app.cfg.general.tab_color = "#123456".into();
        assert_eq!(app.entry_color(&entry(SessionRef::New, Harness::Pi)), "#123456");
    }

    #[test]
    fn entry_modal_placeholder_follows_the_agent() {
        let (mut app, _a, _b) = mixed("entry-placeholder");
        app.on_key(ch('n'));
        app.on_key(key(KeyCode::Down));
        app.on_key(ch('a'));
        app.on_key(key(KeyCode::Enter)); // beta
        app.on_key(key(KeyCode::Enter)); // «Nueva sesión» → modal de la entrada
        let placeholder = |app: &App| match &options(app).field(FieldId::Title).unwrap().kind {
            FieldKind::Text { placeholder, .. } => placeholder.clone(),
            _ => unreachable!(),
        };
        assert_eq!(placeholder(&app), "beta · nueva");
        let agent = options(&app).fields.iter().position(|f| f.id == FieldId::Harness).unwrap();
        for _ in 0..agent {
            app.on_key(key(KeyCode::Down));
        }
        app.on_key(key(KeyCode::Right)); // Codex
        assert_eq!(placeholder(&app), "beta · codex · nueva");
    }

    #[test]
    fn existing_session_keeps_its_agent() {
        let (mut app, alpha, _beta) = mixed("agent-existing");
        // Recientes: x1 (Codex) es la más reciente.
        app.on_key(ch('2'));
        app.on_key(ch('o'));
        let m = options(&app);
        assert_eq!(m.harness, Harness::Codex);
        assert!(m.field(FieldId::Harness).is_none(), "una sesión existente no cambia de agente");
        assert_fields_match(m, Harness::Codex, true);
        app.on_key(key(KeyCode::Esc));

        // Abrir la de Pi: su harness, su carpeta y su archivo de sesión.
        let i = app.catalog.by_id["p1"];
        let file = app.catalog.sessions[i].meta.file.to_string_lossy().into_owned();
        app.launch_session(i);
        let (items, _, _) = &launched(&app)[0];
        assert_eq!(items[0].harness, Harness::Pi);
        assert_eq!(items[0].cwd, paths::natural(&alpha));
        assert!(items[0].args.contains(&file), "{:?}", items[0].args);
        assert_eq!(items[0].session_id.as_deref(), Some("p1"));
    }

    #[test]
    fn sets_remember_the_agent() {
        let (mut app, _alpha, beta) = mixed("set-agent");
        app.on_key(ch('n'));
        app.on_key(key(KeyCode::Down)); // sale del nombre
        app.on_key(ch('a')); // elegir repo
        app.on_key(key(KeyCode::Enter)); // beta (el más reciente)
        app.on_key(key(KeyCode::Tab)); // agente: Codex
        app.on_key(key(KeyCode::Down)); // «Última sesión»
        app.on_key(key(KeyCode::Enter));
        assert_eq!(options(&app).harness, Harness::Codex);
        assert!(options(&app).field(FieldId::Harness).is_some());
        app.on_key(key(KeyCode::Enter)); // guardar
        let entry = &app.cfg.sets[0].entries[0];
        assert_eq!((entry.harness, &entry.session), (Harness::Codex, &SessionRef::Latest));
        // El color por defecto del harness no se guarda en la entrada.
        assert_eq!(entry.tab_color, None);

        // «Última» es la última de ese agente en la carpeta: x1, no la s3 de Claude.
        app.launch_set(0);
        let (items, _, _) = &launched(&app)[0];
        assert_eq!(items[0].harness, Harness::Codex);
        assert_eq!(items[0].session_id.as_deref(), Some("x1"));
        assert_eq!(items[0].cwd, paths::natural(&beta));
        assert_eq!(items[0].tab_color.as_deref(), Some(Harness::Codex.info().color));

        // Sin sesiones de ese agente en la carpeta, abre una nueva con él.
        app.cfg.sets[0].entries[0].harness = Harness::Antigravity;
        app.launch_set(0);
        let (items, _, _) = &launched(&app)[1];
        assert_eq!(items[0].harness, Harness::Antigravity);
        let (new_args, _) = Harness::Antigravity.args(&Mode::New, &LaunchOptions::default(), None);
        assert_eq!(items[0].args, new_args);

        // Una sesión concreta manda sobre el harness que diga la entrada.
        app.cfg.sets[0].entries[0].session = SessionRef::Id("s3".into());
        app.launch_set(0);
        let (items, _, _) = &launched(&app)[2];
        assert_eq!(items[0].harness, Harness::Claude);
        assert_eq!(items[0].args, vec!["-r", "s3"]);
    }

    #[test]
    fn guard_without_fork_only_offers_to_open_anyway() {
        // Un harness que no sabe bifurcar desde la línea de comandos.
        let Some(h) = Harness::ALL.into_iter().find(|h| !h.info().fork) else { return };
        let (mut app, _a, _b) = fixture_with("guard-no-fork", &Harness::ALL, &[("n1", h, true, 200_000, "Sin fork")]);
        let mut live = HashMap::new();
        live.insert("n1".to_string(), LiveSession { harness: h, pid: 7, session_id: "n1".into(), entrypoint: "cli".into(), ..Default::default() });
        app.live = Arc::new(live);
        app.on_key(ch('2')); // Recientes: n1 es la más reciente
        app.on_key(key(KeyCode::Enter));
        let Some(Modal::Guard(g)) = &app.modal else { panic!("se esperaba la guarda") };
        assert!(!g.can_fork);
        assert_eq!(g.choices(), [GuardChoice::OpenAnyway, GuardChoice::Cancel]);
        assert!(!g.conflicts[0].detail.contains("inactiva"), "solo se sabe que está abierta: {}", g.conflicts[0].detail);
        app.on_key(ch('f')); // sin fork, la tecla no hace nada
        assert!(matches!(app.modal, Some(Modal::Guard(_))));
        assert!(launched(&app).is_empty());
        app.on_key(key(KeyCode::Enter)); // primera opción: abrir igual
        let (items, _, _) = &launched(&app)[0];
        assert_eq!(items[0].harness, h);
        assert_eq!(items[0].session_id.as_deref(), Some("n1"));
    }

    #[test]
    fn filter_finds_sessions_by_agent() {
        let (app, _a, _b) = mixed("filter-agent");
        let ids = |f: &str| -> Vec<String> { app.recent_rows(f).into_iter().map(|i| app.catalog.sessions[i].meta.id.clone()).collect() };
        assert_eq!(ids("codex"), ["x1"]);
        assert_eq!(ids("claude").len(), 3);
        assert_eq!(ids("pi alfa"), ["p1"]);
        // Con un solo harness la palabra no filtra nada (no está en ningún título).
        let (solo, _a, _b) = fixture("filter-agent-solo");
        assert!(solo.recent_rows("claude").is_empty());
    }
}
