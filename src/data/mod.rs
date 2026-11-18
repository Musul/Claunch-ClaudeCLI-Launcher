//! Catálogo de repos y sesiones, y el hilo que lo mantiene al día.

pub mod graph;
pub mod live;
pub mod repos;
pub mod sessions;
pub mod usage;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::harness::{Harness, Prev};
use crate::paths;
pub use live::LiveSession;
pub use repos::RootKind;
pub use sessions::SessionMeta;
pub use usage::UsageState;

/// `uds:<tubería>` / `bridge:<id>` → id de sesión local, aprendido de las
/// sesiones vivas (sirve para reconocer remitentes ya cerrados).
pub type Aliases = HashMap<String, String>;

#[derive(Debug, Clone)]
pub struct Session {
    pub meta: SessionMeta,
    pub repo_key: String,
    /// Carpeta donde se reanuda, con la grafía que espera su harness (Claude:
    /// unidad en minúscula; el resto: como la da el sistema).
    pub launch_cwd: PathBuf,
    pub cwd_exists: bool,
    pub subpath: Option<String>,
    pub worktree: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Repo {
    pub key: String,
    pub root: PathBuf,
    pub name: String,
    pub kind: RootKind,
    pub exists: bool,
    pub last_activity_ms: i64,
    /// Índices en `Catalog::sessions`, de la más reciente a la más antigua.
    pub sessions: Vec<usize>,
}

#[derive(Debug, Default)]
pub struct Catalog {
    /// Todas las sesiones, de la más reciente a la más antigua.
    pub sessions: Vec<Session>,
    pub by_id: HashMap<String, usize>,
    pub repos: Vec<Repo>,
    pub repo_by_key: HashMap<String, usize>,
    /// Harness con al menos una sesión, en su orden habitual.
    pub harnesses: Vec<Harness>,
}

impl Catalog {
    pub fn build(metas: &[SessionMeta], scanned: &[PathBuf]) -> Catalog {
        let mut metas: Vec<&SessionMeta> = metas.iter().filter(|m| m.has_messages && !m.hidden && m.cwd.is_some()).collect();
        metas.sort_by_key(|m| std::cmp::Reverse(m.mtime_ms));

        let cwds: Vec<String> = {
            let mut seen = HashSet::new();
            metas
                .iter()
                .filter_map(|m| m.cwd.clone())
                .filter(|c| seen.insert(paths::key(Path::new(c))))
                .collect()
        };
        let placements = repos::place_all(&cwds, scanned);

        let mut repo_map: HashMap<String, Repo> = HashMap::new();
        let new_repo = |root: &Path, kind: RootKind| Repo {
            key: paths::key(root),
            root: paths::normalize(root),
            name: root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.display().to_string()),
            kind,
            exists: root.is_dir(),
            last_activity_ms: 0,
            sessions: Vec::new(),
        };
        for root in scanned {
            repo_map.entry(paths::key(root)).or_insert_with(|| new_repo(root, RootKind::Git));
        }

        let mut sessions = Vec::with_capacity(metas.len());
        for m in metas {
            let cwd = m.cwd.as_deref().unwrap_or_default();
            let Some(p) = placements.get(&paths::key(Path::new(cwd))) else { continue };
            let repo_key = paths::key(&p.root);
            let repo = repo_map.entry(repo_key.clone()).or_insert_with(|| new_repo(&p.root, p.kind.clone()));
            repo.sessions.push(sessions.len());
            repo.last_activity_ms = repo.last_activity_ms.max(m.mtime_ms);
            let launch_cwd = m.harness.launch_cwd(Path::new(cwd));
            sessions.push(Session {
                cwd_exists: launch_cwd.is_dir(),
                launch_cwd,
                meta: m.clone(),
                repo_key,
                subpath: p.subpath.clone(),
                worktree: p.worktree.clone(),
            });
        }

        let mut repo_list: Vec<Repo> = repo_map.into_values().collect();
        repo_list.par_iter_mut().for_each(|r| {
            if r.kind == RootKind::Git
                && let Some(t) = repos::git_activity_ms(&r.root) {
                    r.last_activity_ms = r.last_activity_ms.max(t);
                }
        });
        repo_list.sort_by(|a, b| {
            b.exists.cmp(&a.exists).then(b.last_activity_ms.cmp(&a.last_activity_ms)).then(a.name.cmp(&b.name))
        });

        // Ante un id repetido gana la sesión más reciente (van de nueva a vieja).
        let mut by_id = HashMap::with_capacity(sessions.len());
        for (i, s) in sessions.iter().enumerate() {
            by_id.entry(s.meta.id.clone()).or_insert(i);
        }
        let repo_by_key = repo_list.iter().enumerate().map(|(i, r)| (r.key.clone(), i)).collect();
        let harnesses = Harness::ALL.into_iter().filter(|h| sessions.iter().any(|s| s.meta.harness == *h)).collect();
        Catalog { sessions, by_id, repos: repo_list, repo_by_key, harnesses }
    }

    /// Hay sesiones de más de un harness: las filas indican de cuál es cada una.
    pub fn mixed(&self) -> bool {
        self.harnesses.len() > 1
    }

    pub fn session(&self, id: &str) -> Option<&Session> {
        self.by_id.get(id).map(|&i| &self.sessions[i])
    }

    pub fn repo(&self, key: &str) -> Option<&Repo> {
        self.repo_by_key.get(key).map(|&i| &self.repos[i])
    }

    /// Repo que contiene la ruta (por clave exacta o por prefijo más largo).
    pub fn repo_for_path(&self, path: &Path) -> Option<&Repo> {
        let k = paths::key(path);
        self.repo(&k).or_else(|| {
            self.repos
                .iter()
                .filter(|r| paths::key_within(&k, &r.key))
                .max_by_key(|r| r.key.len())
        })
    }
}

// ---------------------------------------------------------------- índice en disco

// 2: añade `links` (mensajes entre sesiones) y `aliases`.
// 3: sesiones de varios harness (`harness`, `model`). Cada cambio obliga a reindexar.
const INDEX_VERSION: u32 = 3;

#[derive(Serialize, Deserialize, Default)]
struct IndexFile {
    version: u32,
    sessions: Vec<SessionMeta>,
    roots: Vec<PathBuf>,
    #[serde(default)]
    aliases: Aliases,
}

fn index_path() -> PathBuf {
    paths::data_dir().join("index.json")
}

/// Lee el índice guardado. El de la versión 2 (solo Claude) se aprovecha tal
/// cual: sus sesiones se leen igual (sin `harness` son de Claude) y conserva
/// los alias aprendidos, que no se pueden volver a deducir de sesiones cerradas.
fn load_index() -> IndexFile {
    let Ok(bytes) = std::fs::read(index_path()) else { return IndexFile::default() };
    parse_index(&bytes)
}

fn parse_index(bytes: &[u8]) -> IndexFile {
    match serde_json::from_slice::<IndexFile>(bytes) {
        Ok(i) if i.version == INDEX_VERSION || i.version == 2 => IndexFile { version: INDEX_VERSION, ..i },
        Ok(_) => IndexFile::default(),
        Err(e) => {
            crate::warn_log!("índice ilegible, se reconstruye: {e}");
            IndexFile::default()
        }
    }
}

fn save_index(sessions: &[SessionMeta], roots: &[PathBuf], aliases: &Aliases) {
    let file = IndexFile { version: INDEX_VERSION, sessions: sessions.to_vec(), roots: roots.to_vec(), aliases: aliases.clone() };
    let result = serde_json::to_vec(&file).map_err(anyhow::Error::from).and_then(|b| crate::util::write_atomic(&index_path(), &b));
    if let Err(e) = result {
        crate::warn_log!("no se pudo guardar el índice: {e}");
    }
}

// ---------------------------------------------------------------- worker

#[derive(Clone)]
pub struct ScanConfig {
    /// Harness activos con su carpeta de datos.
    pub sources: Vec<(Harness, PathBuf)>,
    pub roots: Vec<PathBuf>,
    pub depth: usize,
    pub exclude: Vec<String>,
    /// Consultar el uso del plan de Claude (`general.show_usage`).
    pub usage: bool,
    /// Carpeta con los avisos de sesiones abiertas por claunch (`live::markers_dir`).
    pub markers: PathBuf,
}

impl ScanConfig {
    /// Carpeta de datos del harness, si está activo.
    pub fn home(&self, h: Harness) -> Option<&Path> {
        self.sources.iter().find(|(x, _)| *x == h).map(|(_, home)| home.as_path())
    }
}

pub enum WorkerMsg {
    Progress { done: usize, total: usize },
    Catalog(Arc<Catalog>),
    Live(Arc<HashMap<String, LiveSession>>),
    Aliases(Arc<Aliases>),
    Usage(UsageState),
}

pub enum WorkerCmd {
    /// Reescanea carpetas raíz y sesiones.
    Refresh,
}

struct State {
    cfg: ScanConfig,
    /// Sesiones de cada harness, por su clave de índice.
    metas: HashMap<Harness, Prev>,
    roots: Vec<PathBuf>,
    aliases: Aliases,
    /// Última escritura del índice; con sesiones activas cambia cada pocos segundos.
    saved_at: Option<Instant>,
}

const SAVE_EVERY: Duration = Duration::from_secs(20);

/// Margen para casar una sesión con el aviso de su proceso: la hora del aviso
/// es anterior al proceso, pero hay harness que fechan sus sesiones con
/// precisión de segundos.
const MARKER_SLACK_MS: i64 = 3_000;

/// Lo que una pasada averigua de un aviso sin sesión.
#[derive(Debug, Default, PartialEq)]
struct Guess {
    /// Su sesión, si solo puede ser suya.
    session: Option<String>,
    /// Sesiones que pudieron ser suyas o de otro aviso: no se le atribuyen,
    /// tampoco cuando el otro ya no esté.
    not_mine: Vec<String>,
}

impl State {
    /// Estado inicial a partir del índice guardado (sin los harness apagados).
    fn new(cfg: ScanConfig, cached: IndexFile) -> State {
        let mut metas: HashMap<Harness, Prev> = HashMap::new();
        for m in cached.sessions {
            if cfg.home(m.harness).is_some() {
                metas.entry(m.harness).or_default().insert(m.harness.index_key(&m), m);
            }
        }
        State { cfg, metas, roots: cached.roots, aliases: cached.aliases, saved_at: None }
    }

    fn all_metas(&self) -> Vec<SessionMeta> {
        self.metas.values().flat_map(|m| m.values().cloned()).collect()
    }

    /// Reindexa lo que cambió en todos los harness; devuelve `true` si algo cambió.
    fn reindex(&mut self, tx: &Sender<WorkerMsg>, report: bool) -> bool {
        let units: Vec<(Harness, &Path, PathBuf)> = self
            .cfg
            .sources
            .iter()
            .flat_map(|(h, home)| h.units(home).into_iter().map(move |u| (*h, home.as_path(), u)))
            .collect();
        let total = units.len();
        // Sin sesiones (instalación nueva) el bucle no avisa nunca: quien espera
        // el progreso (`claunch open`, `snapshot`) se quedaría bloqueado 2 min.
        if report && total == 0 {
            let _ = tx.send(WorkerMsg::Progress { done: 0, total: 0 });
        }
        let done = AtomicUsize::new(0);
        let last_report = std::sync::Mutex::new(Instant::now());
        let none = Prev::new();
        let results: Vec<(Harness, Vec<(PathBuf, SessionMeta)>)> = units
            .par_iter()
            .map(|(h, home, unit)| {
                let found = h.index(home, unit, self.metas.get(h).unwrap_or(&none));
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if report
                    && let Ok(mut t) = last_report.lock()
                        && (t.elapsed() > Duration::from_millis(60) || n == total) {
                            *t = Instant::now();
                            let _ = tx.send(WorkerMsg::Progress { done: n, total });
                        }
                (*h, found)
            })
            .collect();
        let mut next: HashMap<Harness, Prev> = HashMap::new();
        for (h, found) in results {
            next.entry(h).or_default().extend(found);
        }
        for (h, home) in &self.cfg.sources {
            if let Some(metas) = next.get_mut(h) {
                h.finish(home, metas);
            }
        }
        next.retain(|_, metas| !metas.is_empty());
        let changed = next != self.metas;
        self.metas = next;
        changed
    }

    fn publish(&mut self, tx: &Sender<WorkerMsg>, force_save: bool) -> bool {
        let metas = self.all_metas();
        let catalog = Catalog::build(&metas, &self.roots);
        if force_save || self.saved_at.is_none_or(|t| t.elapsed() >= SAVE_EVERY) {
            save_index(&metas, &self.roots, &self.aliases);
            self.saved_at = Some(Instant::now());
        }
        tx.send(WorkerMsg::Catalog(Arc::new(catalog))).is_ok()
    }

    /// Sesiones vivas: las que publica Claude (con sus subagentes activos; de
    /// ellas se aprenden alias nuevos) y las que claunch abrió con otro harness.
    fn scan_live(&mut self) -> (HashMap<String, LiveSession>, bool) {
        let mut live = self.cfg.home(Harness::Claude).map(live::scan).unwrap_or_default();
        let mut learned = false;
        let claude = self.metas.get(&Harness::Claude);
        for l in live.values_mut() {
            for alias in [l.socket.as_ref().map(|s| format!("uds:{s}")), l.bridge_id.as_ref().map(|b| format!("bridge:{b}"))]
                .into_iter()
                .flatten()
            {
                if self.aliases.get(&alias) != Some(&l.session_id) {
                    self.aliases.insert(alias, l.session_id.clone());
                    learned = true;
                }
            }
            let file = claude.and_then(|metas| metas.values().find(|m| m.id == l.session_id)).map(|m| m.file.clone());
            l.subagents = file.map(|f| active_subagents(&f)).unwrap_or(0);
        }
        let markers = live::scan_markers(&self.cfg.markers).into_iter().filter(|m| self.cfg.home(m.harness).is_some()).collect();
        self.add_markers(&mut live, markers);
        (live, learned)
    }

    /// Añade a `live` las sesiones que claunch abrió con un harness que no
    /// publica las suyas: primero los avisos que conocen su sesión y luego los
    /// de sesiones nuevas sin id.
    fn add_markers(&self, live: &mut HashMap<String, LiveSession>, markers: Vec<live::Marker>) {
        let (known, unknown): (Vec<_>, Vec<_>) = markers.into_iter().partition(|m| m.session_id.is_some());
        for m in known {
            let id = m.session_id.clone();
            self.add_marker(live, m, id);
        }
        let guesses = self.guess_sessions(&unknown, live);
        for (mut m, guess) in unknown.into_iter().zip(guesses) {
            // Lo averiguado se apunta en el aviso: vale para las siguientes
            // pasadas y para otras TUI, y no cambia cuando se cierra otra pestaña.
            if guess != Guess::default() {
                m.session_id = guess.session;
                m.not_mine.extend(guess.not_mine);
                live::save(&self.cfg.markers, &m);
            }
            let id = m.session_id.clone();
            self.add_marker(live, m, id);
        }
    }

    /// Añade a `live` la sesión de un aviso. Sin sesión conocida cuenta como
    /// abierta con un id provisional.
    fn add_marker(&self, live: &mut HashMap<String, LiveSession>, m: live::Marker, session: Option<String>) {
        // El proceso vive: lo último que guardó el harness dice si trabaja,
        // pero solo si lo guardó este proceso. Un «trabajando» anterior a su
        // arranque es de un proceso que murió a medias. Sin sesión en el
        // índice solo se sabe que está abierta.
        let status = session
            .as_ref()
            .and_then(|id| self.metas.get(&m.harness)?.values().find(|s| &s.id == id))
            .map(|s| if s.busy && s.mtime_ms >= m.started_ms - MARKER_SLACK_MS { "busy" } else { "idle" })
            .unwrap_or_default();
        let id = session.unwrap_or_else(|| format!("claunch:{}", m.pid));
        live.entry(id.clone()).or_insert(LiveSession {
            harness: m.harness,
            pid: m.pid,
            session_id: id,
            cwd: m.cwd,
            entrypoint: "cli".into(),
            status: status.into(),
            name: Some(m.title).filter(|t| !t.is_empty()),
            started_ms: m.started_ms,
            ..Default::default()
        });
    }

    /// Sesión de cada aviso sin id (sesiones nuevas de un harness que no deja
    /// fijarlo), en el mismo orden que `markers`.
    ///
    /// Se recorren por orden de creación las sesiones de terminal nacidas
    /// desde que arrancó el aviso más antiguo. Cada una pudo crearla cualquier
    /// proceso de su harness y carpeta que ya hubiera arrancado y siga sin
    /// sesión. Es suya si solo hay uno, o si solo uno acababa de arrancar
    /// cuando nació ([`Harness::first_session_window_ms`]). Si no, no se
    /// adivina: se descarta para todos y esos procesos siguen abiertos con id
    /// provisional, que es preferible a atribuir a una pestaña la sesión de otra.
    fn guess_sessions(&self, markers: &[live::Marker], taken: &HashMap<String, LiveSession>) -> Vec<Guess> {
        let mut out: Vec<Guess> = markers.iter().map(|_| Guess::default()).collect();
        let Some(earliest) = markers.iter().map(|m| m.started_ms).min() else { return out };
        let harnesses: HashSet<Harness> = markers.iter().map(|m| m.harness).collect();
        let mut sessions: Vec<(i64, &SessionMeta)> = harnesses
            .iter()
            .filter_map(|h| self.metas.get(h))
            .flat_map(|m| m.values())
            .filter(|s| !s.hidden && s.cwd.is_some() && !taken.contains_key(&s.id))
            // Las creadas desde un IDE o una aplicación no son de una pestaña.
            .filter(|s| s.entrypoint.as_deref().is_none_or(|e| e == "cli"))
            .filter_map(|s| Some((s.started_ms?, s)))
            .filter(|(started, _)| *started >= earliest - MARKER_SLACK_MS)
            .collect();
        sessions.sort_by(|a, b| (a.0, &a.1.id).cmp(&(b.0, &b.1.id)));
        for (started, s) in sessions {
            let cwd = s.cwd.as_deref().unwrap_or_default();
            let could: Vec<usize> = (0..markers.len())
                .filter(|&i| {
                    let m = &markers[i];
                    out[i].session.is_none()
                        && m.harness == s.harness
                        && started >= m.started_ms - MARKER_SLACK_MS
                        && !m.not_mine.contains(&s.id)
                        && m.in_cwd(cwd)
                })
                .collect();
            let window = s.harness.first_session_window_ms();
            let recent: Vec<usize> = could.iter().copied().filter(|&i| started <= markers[i].started_ms + window).collect();
            let owner = match (&recent[..], &could[..]) {
                ([i], _) | ([], [i]) => Some(*i),
                _ => None,
            };
            for i in could {
                if Some(i) == owner {
                    out[i].session = Some(s.id.clone());
                } else {
                    out[i].not_mine.push(s.id.clone());
                }
            }
        }
        out
    }
}

/// Subagente activo: su `.jsonl` cambió hace menos de esto.
const SUBAGENT_ACTIVE: Duration = Duration::from_secs(120);

/// Cuenta `<sesión>/subagents/*.jsonl` modificados hace poco.
fn active_subagents(session_file: &Path) -> usize {
    let dir = session_file.with_extension("").join("subagents");
    let Ok(entries) = std::fs::read_dir(&dir) else { return 0 };
    let now = std::time::SystemTime::now();
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age < SUBAGENT_ACTIVE)
        })
        .count()
}

pub fn spawn_worker(cfg: ScanConfig) -> (Receiver<WorkerMsg>, Sender<WorkerCmd>) {
    let (tx, rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel();
    // El uso del plan es de Claude: sin su carpeta de datos no hay nada que consultar.
    match cfg.home(Harness::Claude).filter(|home| cfg.usage && home.is_dir()) {
        Some(home) => usage::spawn(home.to_path_buf(), tx.clone()),
        None => {
            let _ = tx.send(WorkerMsg::Usage(UsageState::Disabled));
        }
    }
    std::thread::Builder::new()
        .name("claunch-index".into())
        .spawn(move || worker(cfg, tx, cmd_rx))
        .expect("no se pudo crear el hilo de índice");
    (rx, cmd_tx)
}

fn worker(cfg: ScanConfig, tx: Sender<WorkerMsg>, cmds: Receiver<WorkerCmd>) {
    let mut st = State::new(cfg, load_index());
    let (mut last_live, _) = st.scan_live();
    let _ = tx.send(WorkerMsg::Live(Arc::new(last_live.clone())));
    let _ = tx.send(WorkerMsg::Aliases(Arc::new(st.aliases.clone())));
    if !st.metas.is_empty() || !st.roots.is_empty() {
        let _ = tx.send(WorkerMsg::Catalog(Arc::new(Catalog::build(&st.all_metas(), &st.roots))));
    }

    let full = |st: &mut State, tx: &Sender<WorkerMsg>| {
        st.reindex(tx, true);
        st.roots = repos::scan_roots(&st.cfg.roots, st.cfg.depth, &st.cfg.exclude);
        st.publish(tx, true)
    };
    if !full(&mut st, &tx) {
        return;
    }

    let mut tick: u32 = 0;
    loop {
        match cmds.recv_timeout(Duration::from_millis(1500)) {
            Ok(WorkerCmd::Refresh) => {
                if !full(&mut st, &tx) {
                    return;
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        tick = tick.wrapping_add(1);
        let (live, learned) = st.scan_live();
        if learned && tx.send(WorkerMsg::Aliases(Arc::new(st.aliases.clone()))).is_err() {
            return;
        }
        if live != last_live {
            last_live = live.clone();
            if tx.send(WorkerMsg::Live(Arc::new(live))).is_err() {
                return;
            }
        }
        if tick.is_multiple_of(2) && st.reindex(&tx, false) && !st.publish(&tx, false) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sin ninguna sesión también hay aviso de progreso: `claunch open` y
    /// `snapshot` lo esperan antes de usar el catálogo.
    fn scan_config(name: &str, harnesses: &[Harness]) -> ScanConfig {
        let base = std::env::temp_dir().join("claunch-tests").join(name);
        let _ = std::fs::remove_dir_all(&base);
        let sources = harnesses.iter().map(|h| (*h, base.join(h.id()))).collect();
        ScanConfig { sources, roots: Vec::new(), depth: 0, exclude: Vec::new(), usage: false, markers: base.join("live") }
    }

    fn meta(h: Harness, id: &str, cwd: &str, started_ms: i64) -> SessionMeta {
        SessionMeta {
            harness: h,
            id: id.into(),
            file: PathBuf::from(format!("{id}.jsonl")),
            cwd: Some(cwd.into()),
            started_ms: Some(started_ms),
            mtime_ms: started_ms,
            has_messages: true,
            ..Default::default()
        }
    }

    /// Sin ninguna sesión también hay aviso de progreso: `claunch open` y
    /// `snapshot` lo esperan antes de usar el catálogo.
    #[test]
    fn empty_projects_still_reports_progress() {
        // Estas variables mandan sobre la carpeta de datos: con ellas definidas,
        // esos harness leerían las sesiones reales de quien ejecuta los tests.
        let redirected = |h: &Harness| match h {
            Harness::Opencode => std::env::var_os("OPENCODE_DB").is_some(),
            Harness::Pi => std::env::var_os("PI_CODING_AGENT_SESSION_DIR").is_some(),
            _ => false,
        };
        let all: Vec<Harness> = Harness::ALL.into_iter().filter(|h| !redirected(h)).collect();
        let cfg = scan_config("empty-home", &all);
        std::fs::create_dir_all(cfg.home(Harness::Claude).unwrap().join("projects")).unwrap();
        let mut st = State::new(cfg, IndexFile::default());
        let (tx, rx) = mpsc::channel();
        assert!(!st.reindex(&tx, true));
        assert!(matches!(rx.try_recv(), Ok(WorkerMsg::Progress { done: 0, total: 0 })));
    }

    #[test]
    fn reindex_reads_claude_sessions_and_notices_changes() {
        let cfg = scan_config("reindex", &[Harness::Claude]);
        let proj = cfg.home(Harness::Claude).unwrap().join("projects").join("c--r-app");
        std::fs::create_dir_all(&proj).unwrap();
        let line = r#"{"type":"user","timestamp":"2026-11-02T14:44:11.943Z","cwd":"/r/app","message":{"role":"user","content":"hola"}}"#;
        std::fs::write(proj.join("s1.jsonl"), format!("{line}\n")).unwrap();
        let mut st = State::new(cfg, IndexFile::default());
        let (tx, _rx) = mpsc::channel();
        assert!(st.reindex(&tx, false));
        assert!(!st.reindex(&tx, false), "sin cambios no hay nada que publicar");
        let metas = st.all_metas();
        assert_eq!(metas.len(), 1);
        assert_eq!((metas[0].harness, metas[0].id.as_str(), metas[0].prompts), (Harness::Claude, "s1", 1));
        std::fs::remove_file(proj.join("s1.jsonl")).unwrap();
        assert!(st.reindex(&tx, false), "una sesión borrada también es un cambio");
        assert!(st.all_metas().is_empty());
    }

    #[test]
    fn cached_sessions_of_disabled_harness_are_dropped() {
        let cached = IndexFile {
            version: INDEX_VERSION,
            sessions: vec![meta(Harness::Claude, "c1", "/r/app", 1), meta(Harness::Codex, "x1", "/r/app", 2)],
            ..Default::default()
        };
        let st = State::new(scan_config("disabled", &[Harness::Claude]), cached);
        let ids: Vec<String> = st.all_metas().into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["c1"]);
    }

    #[test]
    fn catalog_mixes_harnesses_and_keeps_cwd_spelling() {
        let dir = std::env::temp_dir().join("claunch-tests").join("mixto");
        std::fs::create_dir_all(&dir).unwrap();
        let cwd = dir.to_string_lossy().into_owned();
        let c = Catalog::build(&[meta(Harness::Claude, "c1", &cwd, 10), meta(Harness::Pi, "p1", &format!("{cwd}{}", paths::SEP), 20)], &[]);
        assert_eq!(c.harnesses, [Harness::Claude, Harness::Pi]);
        assert!(c.mixed());
        // Las dos caen en el mismo repo, la más reciente primero.
        assert_eq!(c.repos.len(), 1);
        assert_eq!(c.sessions[0].meta.id, "p1");
        // Claude se reanuda con la unidad en minúscula; el resto, como la da el sistema.
        assert_eq!(c.session("c1").unwrap().launch_cwd, paths::normalize(&dir));
        assert_eq!(c.session("p1").unwrap().launch_cwd, paths::natural(&dir));
        assert!(!Catalog::build(&[meta(Harness::Codex, "x", &cwd, 1)], &[]).mixed());
        // Las ocultas (subagentes, archivadas) siguen en el índice pero no se listan.
        let hidden = SessionMeta { hidden: true, ..meta(Harness::Codex, "hija", &cwd, 30) };
        let c = Catalog::build(&[meta(Harness::Claude, "c1", &cwd, 10), hidden], &[]);
        assert_eq!(c.sessions.len(), 1);
        assert_eq!(c.harnesses, [Harness::Claude]);
    }

    #[test]
    fn markers_become_live_sessions() {
        let cfg = scan_config("markers", &[Harness::Claude, Harness::Codex]);
        std::fs::create_dir_all(&cfg.markers).unwrap();
        let me = std::process::id();
        let marker = live::Marker {
            harness: Harness::Codex,
            pid: me,
            proc_start: live::process_started(me),
            cwd: "/r/app".into(),
            title: "App · nueva".into(),
            started_ms: 100_000,
            ..Default::default()
        };
        std::fs::write(cfg.markers.join(format!("{me}.json")), serde_json::to_vec(&marker).unwrap()).unwrap();
        let cached = IndexFile {
            version: INDEX_VERSION,
            sessions: vec![
                meta(Harness::Codex, "vieja", "/r/app", 50_000),
                meta(Harness::Codex, "otra-carpeta", "/r/api", 101_000),
                meta(Harness::Claude, "de-claude", "/r/app", 101_000),
            ],
            ..Default::default()
        };
        let mut st = State::new(cfg, cached);
        if !cfg!(any(windows, target_os = "linux")) {
            return;
        }
        // Aún no existe la sesión: abierta con id provisional.
        let (live, _) = st.scan_live();
        let l = &live[&format!("claunch:{me}")];
        assert_eq!((l.harness, l.pid, l.status.as_str(), l.name.as_deref()), (Harness::Codex, me, "", Some("App · nueva")));
        assert!(!l.is_busy() && !l.is_waiting());

        // Aparece la sesión creada tras el arranque: el aviso pasa a ser suyo,
        // con el estado que dejó guardado el harness.
        let nueva = SessionMeta { busy: true, ..meta(Harness::Codex, "nueva", "/r/app", 99_000) };
        st.metas.entry(Harness::Codex).or_default().insert(Harness::Codex.index_key(&nueva), nueva);
        let (live, _) = st.scan_live();
        assert_eq!(live.keys().collect::<Vec<_>>(), ["nueva"]);
        assert!(live["nueva"].is_busy());
        // Queda apuntado en el aviso: ya no depende de qué más haya abierto.
        let saved: live::Marker = serde_json::from_slice(&std::fs::read(st.cfg.markers.join(format!("{me}.json"))).unwrap()).unwrap();
        assert_eq!(saved, live::Marker { session_id: Some("nueva".into()), ..marker.clone() });

        // Con id conocido no hay nada que adivinar.
        let known = live::Marker { session_id: Some("vieja".into()), ..marker };
        std::fs::write(st.cfg.markers.join(format!("{me}.json")), serde_json::to_vec(&known).unwrap()).unwrap();
        let (live, _) = st.scan_live();
        assert_eq!(live.keys().collect::<Vec<_>>(), ["vieja"]);
        assert_eq!(live["vieja"].status, "idle");
    }

    /// Estado con solo Codex, y lo necesario para probar el casado de avisos.
    fn codex_state(name: &str) -> State {
        State::new(scan_config(name, &[Harness::Codex]), IndexFile::default())
    }

    fn add_session(st: &mut State, id: &str, cwd: &str, started: i64) {
        let m = meta(Harness::Codex, id, cwd, started);
        st.metas.entry(Harness::Codex).or_default().insert(Harness::Codex.index_key(&m), m);
    }

    fn codex_marker(pid: u32, cwd: &str, started_ms: i64) -> live::Marker {
        live::Marker { harness: Harness::Codex, pid, cwd: cwd.into(), started_ms, ..Default::default() }
    }

    const MIN: i64 = 60_000;

    #[test]
    fn markers_in_the_same_folder_are_matched_only_without_doubt() {
        let free = HashMap::new();
        let is = |id: &str| Guess { session: Some(id.into()), not_mine: Vec::new() };
        let not = |id: &str| Guess { session: None, not_mine: vec![id.into()] };
        let (a, b) = (codex_marker(1, "/r/app", 100_000), codex_marker(2, "/r/app", 105_000));
        let late = codex_marker(4, "/r/app", 100_000 + 60 * MIN);

        // Dos procesos abiertos casi a la vez en la misma carpeta: la sesión
        // que aparezca puede ser de cualquiera, así que no es de ninguno.
        let mut st = codex_state("markers-grupo");
        add_session(&mut st, "s1", "/r/app", 120_000);
        assert_eq!(st.guess_sessions(&[a.clone(), b.clone()], &free), [not("s1"), not("s1")]);
        // Otra carpeta no se ve afectada.
        add_session(&mut st, "s-api", "/r/api", 110_000);
        let c = codex_marker(3, "/r/api", 104_000);
        assert_eq!(st.guess_sessions(&[a.clone(), c, b.clone()], &free), [not("s1"), is("s-api"), not("s1")]);
        // Con el descarte apuntado, la sesión dudosa no pasa al que queda cuando
        // el otro se cierra; la siguiente, que ya solo puede ser suya, sí.
        let a_noted = live::Marker { not_mine: vec!["s1".into()], ..a.clone() };
        assert_eq!(st.guess_sessions(std::slice::from_ref(&a_noted), &free), [Guess::default()]);
        add_session(&mut st, "s3", "/r/app", 130_000);
        assert_eq!(st.guess_sessions(&[a_noted], &free), [is("s3")]);

        // Abiertos con tiempo de por medio y usados en orden: cada uno con la
        // suya, sea cual sea el orden de los avisos.
        let mut st = codex_state("markers-orden");
        add_session(&mut st, "s1", "/r/app", 120_000);
        add_session(&mut st, "s2", "/r/app", 100_000 + 61 * MIN);
        assert_eq!(st.guess_sessions(&[a.clone(), late.clone()], &free), [is("s1"), is("s2")]);
        assert_eq!(st.guess_sessions(&[late.clone(), a.clone()], &free), [is("s2"), is("s1")]);
        // Una sesión que ya tiene proceso no se reparte otra vez.
        let taken: HashMap<String, LiveSession> = [("s2".to_string(), LiveSession::default())].into();
        assert_eq!(st.guess_sessions(std::slice::from_ref(&late), &taken), [Guess::default()]);

        // El primero lleva rato sin usar: lo que nace justo al arrancar el
        // segundo es del segundo, y el primero lo descarta para no heredarlo.
        let birth = Harness::Codex.first_session_window_ms();
        let mut st = codex_state("markers-sin-usar");
        add_session(&mut st, "s2", "/r/app", 100_000 + 60 * MIN + birth);
        assert_eq!(st.guess_sessions(&[a.clone(), late.clone()], &free), [not("s2"), is("s2")]);
        // Cuando por fin se usa el primero, su sesión es suya.
        let a_noted = live::Marker { not_mine: vec!["s2".into()], ..a.clone() };
        add_session(&mut st, "s4", "/r/app", 100_000 + 70 * MIN);
        assert_eq!(st.guess_sessions(&[a_noted], &taken), [is("s4")]);
        // Pasado ese momento ya no se sabe en cuál de las dos se escribió: la
        // sesión que nace después no es de nadie.
        let mut st = codex_state("markers-rato");
        add_session(&mut st, "s5", "/r/app", 100_000 + 60 * MIN + birth + 1);
        assert_eq!(st.guess_sessions(&[a.clone(), late.clone()], &free), [not("s5"), not("s5")]);

        // Un harness que crea la sesión con el primer prompt da más margen a
        // la pestaña recién abierta: lo normal es abrirla para usarla.
        let opencode = |m: &live::Marker| live::Marker { harness: Harness::Opencode, ..m.clone() };
        let mut st = State::new(scan_config("markers-primer-prompt", &[Harness::Opencode]), IndexFile::default());
        let mut add = |id: &str, started: i64| {
            let m = meta(Harness::Opencode, id, "/r/app", started);
            st.metas.entry(Harness::Opencode).or_default().insert(Harness::Opencode.index_key(&m), m);
        };
        add("o1", 100_000 + 60 * MIN + 5 * MIN);
        add("o2", 100_000 + 60 * MIN + Harness::Opencode.first_session_window_ms() + 1);
        assert!(Harness::Opencode.first_session_window_ms() > 5 * MIN && birth < MIN);
        let both = st.guess_sessions(&[opencode(&a), opencode(&late)], &free);
        assert_eq!(both, [Guess { session: Some("o2".into()), not_mine: vec!["o1".into()] }, is("o1")]);
    }

    #[test]
    fn a_lone_marker_takes_the_first_session_created_after_it() {
        let free = HashMap::new();
        let a = codex_marker(1, "/r/app", 100_000);
        let guess = |st: &State| st.guess_sessions(std::slice::from_ref(&a), &free).remove(0).session;
        let each = |st: &mut State, f: fn(&mut SessionMeta)| st.metas.get_mut(&Harness::Codex).unwrap().values_mut().for_each(f);
        let mut st = codex_state("markers-solo");
        assert_eq!(st.guess_sessions(&[], &free), []);
        // Las anteriores a su arranque no son suyas.
        add_session(&mut st, "antes", "/r/app", 50_000);
        assert_eq!(guess(&st), None);
        // La suya lo es aunque el primer prompt tarde horas.
        add_session(&mut st, "tarde", "/r/app", 100_000 + 180 * MIN);
        assert_eq!(guess(&st).as_deref(), Some("tarde"));
        // No cuentan las creadas desde un IDE, las ocultas ni las que no tienen fecha.
        each(&mut st, |m| m.entrypoint = Some("vscode".into()));
        assert_eq!(guess(&st), None);
        each(&mut st, |m| (m.entrypoint, m.hidden) = (Some("cli".into()), true));
        assert_eq!(guess(&st), None);
        each(&mut st, |m| (m.hidden, m.started_ms) = (false, None));
        assert_eq!(guess(&st), None);
        // El margen del reloj cuenta y, de dos candidatas, gana la más antigua.
        each(&mut st, |m| m.started_ms = Some(100_000 - MARKER_SLACK_MS));
        assert_eq!(guess(&st).as_deref(), Some("antes"));
        each(&mut st, |m| m.started_ms = Some(100_000 - MARKER_SLACK_MS - 1));
        assert_eq!(guess(&st), None);
    }

    #[test]
    fn what_is_learned_about_a_marker_is_written_to_it() {
        let mut st = codex_state("markers-apuntes");
        let dir = st.cfg.markers.clone();
        std::fs::create_dir_all(&dir).unwrap();
        let path = |pid: u32| dir.join(format!("{pid}.json"));
        let read = |pid: u32| serde_json::from_slice::<live::Marker>(&std::fs::read(path(pid)).unwrap()).unwrap();
        let (a, b) = (codex_marker(1, "/r/app", 100_000), codex_marker(2, "/r/app", 100_000 + 60 * MIN));
        for m in [&a, &b] {
            std::fs::write(path(m.pid), serde_json::to_vec(m).unwrap()).unwrap();
        }
        let keys = |live: &HashMap<String, LiveSession>| {
            let mut k: Vec<String> = live.keys().cloned().collect();
            k.sort();
            k
        };

        // Sin sesiones: las dos pestañas, abiertas con id provisional y sin apuntes.
        let mut live = HashMap::new();
        st.add_markers(&mut live, vec![a.clone(), b.clone()]);
        assert_eq!(keys(&live), ["claunch:1", "claunch:2"]);
        assert_eq!((read(1), read(2)), (a.clone(), b.clone()));

        // La segunda crea su sesión al arrancar: se le apunta, y a la primera, que no es suya.
        add_session(&mut st, "s2", "/r/app", 100_000 + 60 * MIN + 5_000);
        let mut live = HashMap::new();
        st.add_markers(&mut live, vec![read(1), read(2)]);
        assert_eq!(keys(&live), ["claunch:1", "s2"]);
        assert_eq!((live["s2"].pid, live["s2"].status.as_str()), (2, "idle"));
        assert_eq!(read(2), live::Marker { session_id: Some("s2".into()), ..b.clone() });
        assert_eq!(read(1), live::Marker { not_mine: vec!["s2".into()], ..a.clone() });

        // Otra pasada no cambia nada, y al cerrarse la segunda su sesión no
        // pasa a la primera: deja de estar abierta.
        let mut live = HashMap::new();
        st.add_markers(&mut live, vec![read(1), read(2)]);
        assert_eq!(keys(&live), ["claunch:1", "s2"]);
        assert_eq!(read(1).not_mine, ["s2"]);
        std::fs::remove_file(path(2)).unwrap();
        let mut live = HashMap::new();
        st.add_markers(&mut live, vec![read(1)]);
        assert_eq!(keys(&live), ["claunch:1"]);
    }

    #[test]
    fn stale_busy_is_not_inherited_by_a_resumed_session() {
        let mut st = State::new(scan_config("busy-viejo", &[Harness::Codex]), IndexFile::default());
        // La sesión quedó «trabajando» cuando su proceso murió, mucho antes de reanudarla.
        let dead = SessionMeta { busy: true, ..meta(Harness::Codex, "s", "/r/app", 1_000) };
        st.metas.entry(Harness::Codex).or_default().insert(Harness::Codex.index_key(&dead), dead);
        let marker = |started_ms: i64| live::Marker { session_id: Some("s".into()), ..codex_marker(9, "/r/app", started_ms) };
        let mut live = HashMap::new();
        st.add_marker(&mut live, marker(500_000), Some("s".into()));
        assert_eq!(live["s"].status, "idle");
        // En cuanto el proceso nuevo escribe, su estado vuelve a contar.
        st.metas.get_mut(&Harness::Codex).unwrap().values_mut().for_each(|m| m.mtime_ms = 600_000);
        let mut live = HashMap::new();
        st.add_marker(&mut live, marker(500_000), Some("s".into()));
        assert!(live["s"].is_busy());
    }

    #[test]
    fn index_of_the_previous_version_is_kept() {
        let v2 = r#"{"version":2,"sessions":[{"id":"s","file":"s.jsonl","size":1,"mtime_ms":2,"indexed_to":0,"fingerprint":0,
            "cwd":"/r","custom_title":null,"ai_title":null,"agent_name":null,"first_prompt":null,"last_prompt":null,"git_branch":null,
            "entrypoint":null,"started_ms":null,"pr_number":null,"prompts":1,"has_messages":true}],"roots":["/r"],"aliases":{"uds:x":"s"}}"#;
        let i = parse_index(v2.as_bytes());
        assert_eq!((i.version, i.sessions.len(), i.roots.len()), (INDEX_VERSION, 1, 1));
        assert_eq!(i.sessions[0].harness, Harness::Claude);
        // Los alias solo se aprenden de sesiones vivas: perderlos no tiene arreglo.
        assert_eq!(i.aliases.get("uds:x").map(String::as_str), Some("s"));
        // Otras versiones, o basura: se reindexa desde cero.
        assert!(parse_index(br#"{"version":1,"sessions":[],"roots":["/r"]}"#).roots.is_empty());
        assert!(parse_index(br#"{"version":99,"sessions":[],"roots":["/r"]}"#).roots.is_empty());
        assert_eq!(parse_index(b"no es json").version, 0);
    }
}
