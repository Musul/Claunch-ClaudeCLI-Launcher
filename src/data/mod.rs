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
    /// `cwd` normalizado (unidad en minúscula): donde se reanuda.
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
}

impl Catalog {
    pub fn build(metas: &[SessionMeta], scanned: &[PathBuf]) -> Catalog {
        let mut metas: Vec<&SessionMeta> = metas.iter().filter(|m| m.has_messages && m.cwd.is_some()).collect();
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
            let launch_cwd = paths::normalize(Path::new(cwd));
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

        let by_id = sessions.iter().enumerate().map(|(i, s)| (s.meta.id.clone(), i)).collect();
        let repo_by_key = repo_list.iter().enumerate().map(|(i, r)| (r.key.clone(), i)).collect();
        Catalog { sessions, by_id, repos: repo_list, repo_by_key }
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
                .filter(|r| k.starts_with(&format!("{}\\", r.key)))
                .max_by_key(|r| r.key.len())
        })
    }
}

// ---------------------------------------------------------------- índice en disco

// 2: añade `links` (mensajes entre sesiones) y `aliases`; obliga a reindexar.
const INDEX_VERSION: u32 = 2;

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

fn load_index() -> IndexFile {
    let Ok(bytes) = std::fs::read(index_path()) else { return IndexFile::default() };
    match serde_json::from_slice::<IndexFile>(&bytes) {
        Ok(i) if i.version == INDEX_VERSION => i,
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
    pub claude_home: PathBuf,
    pub roots: Vec<PathBuf>,
    pub depth: usize,
    pub exclude: Vec<String>,
    /// Consultar el uso del plan (`general.show_usage`).
    pub usage: bool,
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
    metas: HashMap<PathBuf, SessionMeta>,
    roots: Vec<PathBuf>,
    aliases: Aliases,
    /// Última escritura del índice; con sesiones activas cambia cada pocos segundos.
    saved_at: Option<Instant>,
}

const SAVE_EVERY: Duration = Duration::from_secs(20);

impl State {
    /// Reindexa lo que cambió; devuelve `true` si algo cambió.
    fn reindex(&mut self, tx: &Sender<WorkerMsg>, report: bool) -> bool {
        let files = sessions::list_session_files(&self.cfg.claude_home.join("projects"));
        let total = files.len();
        // Sin sesiones (instalación nueva) el bucle no avisa nunca: quien espera
        // el progreso (`claunch open`, `snapshot`) se quedaría bloqueado 2 min.
        if report && total == 0 {
            let _ = tx.send(WorkerMsg::Progress { done: 0, total: 0 });
        }
        let done = AtomicUsize::new(0);
        let last_report = std::sync::Mutex::new(Instant::now());
        let results: Vec<(PathBuf, Option<SessionMeta>)> = files
            .par_iter()
            .map(|f| {
                let prev = self.metas.get(f);
                let r = match sessions::index_file(f, prev) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        crate::warn_log!("no se pudo indexar {}: {e}", f.display());
                        prev.cloned()
                    }
                };
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if report
                    && let Ok(mut t) = last_report.lock()
                        && (t.elapsed() > Duration::from_millis(60) || n == total) {
                            *t = Instant::now();
                            let _ = tx.send(WorkerMsg::Progress { done: n, total });
                        }
                (f.clone(), r)
            })
            .collect();
        let mut changed = results.len() != self.metas.len();
        let mut next = HashMap::with_capacity(results.len());
        for (f, m) in results {
            let Some(m) = m else { continue };
            if !changed && self.metas.get(&f) != Some(&m) {
                changed = true;
            }
            next.insert(f, m);
        }
        self.metas = next;
        changed
    }

    fn publish(&mut self, tx: &Sender<WorkerMsg>, force_save: bool) -> bool {
        let metas: Vec<SessionMeta> = self.metas.values().cloned().collect();
        let catalog = Catalog::build(&metas, &self.roots);
        if force_save || self.saved_at.is_none_or(|t| t.elapsed() >= SAVE_EVERY) {
            save_index(&metas, &self.roots, &self.aliases);
            self.saved_at = Some(Instant::now());
        }
        tx.send(WorkerMsg::Catalog(Arc::new(catalog))).is_ok()
    }

    /// Sesiones vivas con sus subagentes activos; aprende alias nuevos.
    fn scan_live(&mut self) -> (HashMap<String, LiveSession>, bool) {
        let mut live = live::scan(&self.cfg.claude_home);
        let mut learned = false;
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
            let file = self.metas.values().find(|m| m.id == l.session_id).map(|m| m.file.clone());
            l.subagents = file.map(|f| active_subagents(&f)).unwrap_or(0);
        }
        (live, learned)
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
    if cfg.usage {
        usage::spawn(cfg.claude_home.clone(), tx.clone());
    } else {
        let _ = tx.send(WorkerMsg::Usage(UsageState::Disabled));
    }
    std::thread::Builder::new()
        .name("claunch-index".into())
        .spawn(move || worker(cfg, tx, cmd_rx))
        .expect("no se pudo crear el hilo de índice");
    (rx, cmd_tx)
}

fn worker(cfg: ScanConfig, tx: Sender<WorkerMsg>, cmds: Receiver<WorkerCmd>) {
    let cached = load_index();
    let mut st = State {
        metas: cached.sessions.into_iter().map(|m| (m.file.clone(), m)).collect(),
        roots: cached.roots,
        aliases: cached.aliases,
        cfg,
        saved_at: None,
    };
    let (mut last_live, _) = st.scan_live();
    let _ = tx.send(WorkerMsg::Live(Arc::new(last_live.clone())));
    let _ = tx.send(WorkerMsg::Aliases(Arc::new(st.aliases.clone())));
    if !st.metas.is_empty() || !st.roots.is_empty() {
        let metas: Vec<SessionMeta> = st.metas.values().cloned().collect();
        let _ = tx.send(WorkerMsg::Catalog(Arc::new(Catalog::build(&metas, &st.roots))));
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
    #[test]
    fn empty_projects_still_reports_progress() {
        let home = std::env::temp_dir().join("claunch-tests").join("empty-home");
        std::fs::create_dir_all(home.join("projects")).unwrap();
        let cfg = ScanConfig { claude_home: home, roots: Vec::new(), depth: 0, exclude: Vec::new(), usage: false };
        let mut st = State { cfg, metas: HashMap::new(), roots: Vec::new(), aliases: Aliases::new(), saved_at: None };
        let (tx, rx) = mpsc::channel();
        st.reindex(&tx, true);
        assert!(matches!(rx.try_recv(), Ok(WorkerMsg::Progress { done: 0, total: 0 })));
    }
}
