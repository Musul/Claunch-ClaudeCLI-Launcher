//! Google Antigravity CLI (`agy`): conversaciones en `~/.gemini/antigravity-cli/`
//! (CLI) y `~/.gemini/antigravity/` (aplicación e IDE).
//!
//! Cada carpeta de datos tiene un índice `conversation_summaries.db` (SQLite) con
//! una fila por conversación: título, vista previa, nº de pasos, última
//! modificación, carpetas de trabajo (`workspace_uris`), estado y un resumen en
//! protobuf (`raw_summary`). Con eso basta para listar. Del archivo de la
//! conversación (`conversations/<id>.db`, SQLite) solo se leen, y solo cuando la
//! fila cambió, el origen, el número de prompts, el último prompt y el modelo.
//! Los `.pb` heredados no se abren nunca: no son legibles.
//!
//! Todo es de solo lectura y para indexar nunca se ejecuta `agy` (arrancarlo,
//! incluso con `--help`, dispara su actualizador y escribe en su carpeta de
//! datos). La única consulta a su `--help` es la de [`preflight`], en la
//! pestaña donde va a arrancar de todos modos.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, Row};

use super::sqlite::{columns, has_table, open_readonly};
use super::{Harness, Info, ModelInput, Perm, Prev};
use crate::config::LaunchOptions;
use crate::data::SessionMeta;
use crate::data::sessions::{clean_prompt, collapse, mtime_ms, set_text};
use crate::launch::Mode;

pub const INFO: Info = Info {
    id: "antigravity",
    label: "Antigravity",
    tag: "agy",
    program: "agy",
    install: "https://antigravity.google",
    color: "#7AAAE0",
    // Solo existe `/fork` dentro de la TUI: no hay flag para bifurcar.
    fork: false,
    // Ni nombre de sesión ni worktree ni id preasignado por flag.
    name: false,
    worktree: false,
    // Un id de `agy models` (depende de la cuenta); `--model` acepta cualquiera.
    model: ModelInput::Free("gemini-3.8-flash-high"),
    permissions: &[
        Perm { value: "", label: "por defecto", hint: "" },
        Perm { value: "accept-edits", label: "accept-edits", hint: "acepta las ediciones, pregunta por los comandos" },
        Perm { value: "auto", label: "auto", hint: "edita en el proyecto y ejecuta comandos en sandbox" },
        Perm { value: "plan", label: "plan", hint: "modo plan" },
        Perm { value: "bypass", label: "bypass", hint: "sin preguntas de permisos" },
    ],
};

/// Carpetas de datos bajo `~/.gemini`: la del CLI y la de la aplicación/IDE.
const DATA_DIRS: [&str; 2] = ["antigravity-cli", "antigravity"];
const SUMMARIES: &str = "conversation_summaries.db";
/// Cada cuánto como mucho se recalculan las conversaciones archivadas.
const ARCHIVE_REFRESH: Duration = Duration::from_secs(30);

/// Carpeta común de Antigravity (`~/.gemini`): dentro están los datos del CLI
/// y los de la aplicación.
pub fn home() -> PathBuf {
    crate::paths::home().join(".gemini")
}

/// Ejecutable de `agy`: el que mantiene al día su actualizador en
/// `~/.gemini/bin`; si no, el del PATH (puede ser un 1.0.x sin `--mode`) y, en
/// Linux, el que deja `install.sh` en `~/.local/bin` (no toca el PATH).
pub fn locate() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "agy.exe" } else { "agy" };
    let updated = home().join("bin").join(exe);
    if updated.is_file() {
        return Some(updated);
    }
    crate::paths::which(INFO.program).or_else(|| {
        let local = crate::paths::home().join(".local").join("bin").join(exe);
        local.is_file().then_some(local)
    })
}

/// Un `agy` que no es el de su actualizador puede ser un 1.0.x, que no tiene
/// `--mode` y se cerraría nada más abrir. Antes de arrancarlo con un modo se
/// mira su `--help` y, si no lo nombra, se abre sin él.
pub fn preflight(program: &Path, args: &mut Vec<String>) -> Option<String> {
    preflight_in(program, args, &crate::paths::launch_dir())
}

/// [`preflight`] con la carpeta donde dejar un momento la salida de `--help`.
fn preflight_in(program: &Path, args: &mut Vec<String>, scratch: &Path) -> Option<String> {
    if !args.iter().any(|a| a == "--mode") {
        return None;
    }
    let updated = home().join("bin");
    if program.parent().is_some_and(|dir| crate::paths::key(dir) == crate::paths::key(&updated)) {
        return None;
    }
    drop_mode(args, &help_of(program, scratch)?)
}

/// Quita `--mode <modo>` si la ayuda de `agy` no lo nombra. Una ayuda que no
/// parece la de `agy` (un comando propio que lo envuelve) no decide nada.
fn drop_mode(args: &mut Vec<String>, help: &str) -> Option<String> {
    // El flag tal cual: `--model` empieza igual que `--mode`.
    let names = |flag: &str| help.match_indices(flag).any(|(i, _)| !help[i + flag.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-'));
    if names("--mode") || !names("--conversation") {
        return None;
    }
    let at = args.iter().position(|a| a == "--mode")?;
    let mode: Vec<String> = args.drain(at..(at + 2).min(args.len())).skip(1).collect();
    Some(format!(
        "este agy es anterior a los modos de permisos: se abre sin «{}», con sus permisos por defecto. Suele actualizarse solo al usarlo.",
        mode.first().map(String::as_str).unwrap_or_default()
    ))
}

/// Lo que imprime `<program> --help`, o `None` si no arranca o tarda. La
/// salida va a un archivo y no a una tubería: si `agy` deja su actualizador
/// en segundo plano con la salida heredada, no se le espera. El archivo se
/// crea en `scratch`, una carpeta del usuario (la de arranques de claunch, que
/// se limpia sola), y nunca sobre uno que ya exista: en la temporal del
/// sistema, compartida en Linux, otro usuario podría dejar ahí un enlace.
fn help_of(program: &Path, scratch: &Path) -> Option<String> {
    use std::process::{Command, Stdio};
    const TIMEOUT: Duration = Duration::from_secs(5);
    static CALLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = scratch.join(format!("agy-help-{}-{n}.txt", std::process::id()));
    let created = std::fs::create_dir_all(scratch).and_then(|()| std::fs::OpenOptions::new().write(true).create_new(true).open(&path));
    let Ok(out) = created else {
        // Sin dónde dejar la salida no se comprueba nada: que al menos conste.
        warn_once(&path, "no se pudo comprobar `--mode`: no se puede escribir");
        return None;
    };
    let spawned = out.try_clone().and_then(|err| Command::new(program).arg("--help").stdin(Stdio::null()).stdout(out).stderr(err).spawn());
    let finished = spawned.ok().is_some_and(|mut child| {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
            }
        }
    });
    let text = std::fs::read(&path);
    let _ = std::fs::remove_file(&path);
    finished.then(|| String::from_utf8_lossy(&text.unwrap_or_default()).into_owned())
}

/// Las bases de índice que existan (CLI y aplicación).
pub fn units(home: &Path) -> Vec<PathBuf> {
    DATA_DIRS.iter().map(|d| home.join(d).join(SUMMARIES)).filter(|p| p.is_file()).collect()
}

/// No hay datos que cruzar entre las dos bases: no hace nada.
pub fn finish(_home: &Path, _metas: &mut Prev) {}

/// Argumentos de `agy`. No hay fork: bifurcar se trata como reanudar.
pub fn args(mode: &Mode, opts: &LaunchOptions) -> (Vec<String>, Option<String>) {
    let mut a = Vec::new();
    let id = match mode {
        Mode::New => None,
        Mode::Resume(id) | Mode::Fork(id) => {
            a.extend(["--conversation".to_string(), id.clone()]);
            Some(id.clone())
        }
    };
    if let Some(m) = opts.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        a.extend(["--model".to_string(), m.to_string()]);
    }
    match opts.permission_mode.as_deref() {
        Some(p @ ("accept-edits" | "auto" | "plan")) => a.extend(["--mode".to_string(), p.to_string()]),
        Some("bypass") => a.push("--dangerously-skip-permissions".to_string()),
        _ => {}
    }
    (a, id)
}

// ---------------------------------------------------------------- índice

/// Fila de `conversation_summaries` (solo lo que se usa).
#[derive(Default)]
struct SummaryRow {
    id: String,
    title: String,
    preview: String,
    steps: i64,
    modified: String,
    uris: String,
    status: String,
    source: String,
    parent: String,
    nesting: i64,
    battle: String,
    killed: i64,
    app_dir: String,
    raw: Vec<u8>,
}

const WANTED: [&str; 14] = [
    "conversation_id",
    "title",
    "preview",
    "step_count",
    "last_modified_time",
    "workspace_uris",
    "status",
    "source",
    "parent_conversation_id",
    "nesting_depth",
    "battle_id",
    "killed",
    "app_data_dir",
    "raw_summary",
];

/// Texto de una celda tolerando cualquier tipo (el esquema no es nuestro).
fn cell_text(r: &Row, i: usize) -> String {
    match r.get_ref(i) {
        Ok(ValueRef::Text(b) | ValueRef::Blob(b)) => String::from_utf8_lossy(b).into_owned(),
        Ok(ValueRef::Integer(n)) => n.to_string(),
        Ok(ValueRef::Real(f)) => f.to_string(),
        _ => String::new(),
    }
}

fn cell_int(r: &Row, i: usize) -> i64 {
    match r.get_ref(i) {
        Ok(ValueRef::Integer(n)) => n,
        Ok(ValueRef::Real(f)) => f as i64,
        Ok(ValueRef::Text(b)) => std::str::from_utf8(b).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0),
        _ => 0,
    }
}

fn cell_blob(r: &Row, i: usize) -> Vec<u8> {
    match r.get_ref(i) {
        Ok(ValueRef::Blob(b) | ValueRef::Text(b)) => b.to_vec(),
        _ => Vec::new(),
    }
}

/// Todas las filas del índice. Las columnas que falten se leen como vacías.
fn read_rows(conn: &Connection) -> rusqlite::Result<Vec<SummaryRow>> {
    let cols = columns(conn, "conversation_summaries");
    if !cols.iter().any(|c| c == "conversation_id") {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let select: Vec<String> =
        WANTED.iter().map(|w| if cols.iter().any(|c| c == w) { format!("\"{w}\"") } else { format!("NULL AS \"{w}\"") }).collect();
    let mut stmt = conn.prepare(&format!("SELECT {} FROM conversation_summaries", select.join(", ")))?;
    let rows = stmt.query_map([], |r| {
        Ok(SummaryRow {
            id: cell_text(r, 0),
            title: cell_text(r, 1),
            preview: cell_text(r, 2),
            steps: cell_int(r, 3),
            modified: cell_text(r, 4),
            uris: cell_text(r, 5),
            status: cell_text(r, 6),
            source: cell_text(r, 7),
            parent: cell_text(r, 8),
            nesting: cell_int(r, 9),
            battle: cell_text(r, 10),
            killed: cell_int(r, 11),
            app_dir: cell_text(r, 12),
            raw: cell_blob(r, 13),
        })
    })?;
    // Una fila ilegible no tumba el resto.
    Ok(rows.flatten().collect())
}

/// Ids de las conversaciones hijas (`conversation_parents`).
fn child_ids(conn: &Connection) -> HashSet<String> {
    let mut out = HashSet::new();
    if !has_table(conn, "conversation_parents") {
        return out;
    }
    if let Ok(mut stmt) = conn.prepare("SELECT conversation_id FROM conversation_parents")
        && let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0))
    {
        out.extend(rows.flatten());
    }
    out
}

type ArchiveCache = HashMap<PathBuf, (u64, Instant, Arc<HashSet<String>>)>;
static ARCHIVED: Mutex<Option<ArchiveCache>> = Mutex::new(None);
/// Bases que ya avisaron de un fallo (para avisar una sola vez).
static WARNED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Huella barata de la base y de su WAL: tamaño y fecha.
fn stamp(unit: &Path) -> u64 {
    let mut h = Fnv::new();
    let mut wal = unit.as_os_str().to_os_string();
    wal.push("-wal");
    for p in [unit.to_path_buf(), PathBuf::from(wal)] {
        match std::fs::metadata(&p) {
            Ok(md) => {
                h.num(md.len() as i64);
                h.num(mtime_ms(&md));
            }
            Err(_) => h.num(-1),
        }
    }
    h.0
}

/// Conversaciones archivadas (cabecera de la tabla FTS con `archived = 1`).
/// Recorrer la FTS no es gratis: se rehace solo si la base cambió y han pasado
/// `ARCHIVE_REFRESH`.
fn archived(conn: &Connection, unit: &Path) -> Arc<HashSet<String>> {
    let now = stamp(unit);
    if let Ok(cache) = ARCHIVED.lock()
        && let Some((s, at, set)) = cache.as_ref().and_then(|c| c.get(unit))
        && (*s == now || at.elapsed() < ARCHIVE_REFRESH)
    {
        return set.clone();
    }
    let mut set = HashSet::new();
    if has_table(conn, "conversations_fts") {
        let found = conn.prepare("SELECT conversation_id FROM conversations_fts WHERE chunk_index = -1 AND archived = 1").and_then(|mut stmt| {
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()
        });
        match found {
            Ok(ids) => set.extend(ids),
            // Un fallo (bloqueo, esquema distinto) no se guarda en la caché: se reintenta en la siguiente
            // pasada y mientras tanto vale lo último que se supo (si no, las archivadas reaparecerían un momento).
            Err(_) => {
                let last = ARCHIVED.lock().ok().and_then(|c| c.as_ref()?.get(unit).map(|e| e.2.clone()));
                return last.unwrap_or_else(|| Arc::new(set));
            }
        }
    }
    let set = Arc::new(set);
    if let Ok(mut cache) = ARCHIVED.lock() {
        cache.get_or_insert_with(HashMap::new).insert(unit.to_path_buf(), (now, Instant::now(), set.clone()));
    }
    set
}

/// Avisa una sola vez por base mientras siga fallando.
fn warn_once(unit: &Path, what: &str) {
    if let Ok(mut w) = WARNED.lock()
        && !w.iter().any(|p| p == unit)
    {
        w.push(unit.to_path_buf());
        crate::warn_log!("Antigravity: {what} {}", unit.display());
    }
}

fn recovered(unit: &Path) {
    if let Ok(mut w) = WARNED.lock() {
        w.retain(|p| p != unit);
    }
}

/// Indexa las conversaciones de una base de índice. Si no se puede leer,
/// conserva las del escaneo anterior de esa carpeta.
pub fn index(_home: &Path, unit: &Path, prev: &Prev) -> Vec<(PathBuf, SessionMeta)> {
    let Some(dir) = unit.parent() else { return Vec::new() };
    let dir_name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let keep = || -> Vec<(PathBuf, SessionMeta)> {
        prev.iter().filter(|(_, m)| m.harness == Harness::Antigravity && m.file.starts_with(dir)).map(|(k, m)| (k.clone(), m.clone())).collect()
    };
    let Ok(conn) = open_readonly(unit) else {
        warn_once(unit, "no se pudo abrir");
        return keep();
    };
    if !has_table(&conn, "conversation_summaries") {
        warn_once(unit, "esquema desconocido en");
        return keep();
    }
    let Ok(rows) = read_rows(&conn) else {
        warn_once(unit, "no se pudo leer");
        return keep();
    };
    recovered(unit);
    let children = child_ids(&conn);
    let archived = archived(&conn, unit);
    let old: HashMap<&str, &SessionMeta> =
        prev.values().filter(|m| m.harness == Harness::Antigravity && m.file.starts_with(dir)).map(|m| (m.id.as_str(), m)).collect();

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        // Espejo de otra carpeta (el CLI replica las filas de la aplicación).
        if row.id.is_empty() || (!row.app_dir.is_empty() && row.app_dir != dir_name) {
            continue;
        }
        // Marcador de posición sin contenido.
        if row.steps == 0 && row.preview.is_empty() && row.title.is_empty() {
            continue;
        }
        let (file, kind) = conversation_file(dir, &row.id);
        let hidden = !row.parent.is_empty()
            || row.nesting > 0
            || !row.battle.is_empty()
            || row.killed != 0
            || children.contains(&row.id)
            || archived.contains(&row.id);
        let fp = fingerprint(&row, kind, hidden);
        let size = file.as_ref().and_then(|f| std::fs::metadata(f).ok()).map_or(0, |m| m.len());
        let key = PathBuf::from(&row.id);
        if let Some(p) = old.get(row.id.as_str()).filter(|p| p.fingerprint == fp) {
            let mut m = (*p).clone();
            m.size = size;
            if let Some(f) = &file {
                m.file = f.clone();
            }
            out.push((key, m));
            continue;
        }
        let origin = Origin { dir_name: &dir_name, unit, file, size, kind, hidden, fp, old: old.get(row.id.as_str()).copied() };
        out.push((key, build(&row, &origin)));
    }
    out
}

/// Archivo de la conversación: `.db` o, si no, el `.pb` heredado (0 = ninguno,
/// 1 = db, 2 = pb). El id sale de una base ajena: solo se usa si es un nombre
/// de archivo simple.
fn conversation_file(dir: &Path, id: &str) -> (Option<PathBuf>, u8) {
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return (None, 0);
    }
    let base = dir.join("conversations");
    for (ext, kind) in [("db", 1), ("pb", 2)] {
        let p = base.join(format!("{id}.{ext}"));
        if p.is_file() {
            return (Some(p), kind);
        }
    }
    (None, 0)
}

/// Huella de una fila: si no cambia (ni el tipo de archivo ni el estado
/// oculto), no hay que releer nada. El 0 queda reservado para «reintentar».
fn fingerprint(r: &SummaryRow, kind: u8, hidden: bool) -> u64 {
    let mut h = Fnv::new();
    for s in [&r.id, &r.title, &r.preview, &r.modified, &r.uris, &r.status, &r.source, &r.app_dir] {
        h.bytes(s.as_bytes());
    }
    h.bytes(&r.raw);
    h.num(r.steps);
    h.num(i64::from(kind));
    h.num(i64::from(hidden));
    h.0.max(1)
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf29ce484222325)
    }

    fn bytes(&mut self, b: &[u8]) {
        for x in b {
            self.0 = (self.0 ^ u64::from(*x)).wrapping_mul(0x100000001b3);
        }
        // Separador, para que ("ab", "c") y ("a", "bc") no coincidan.
        self.0 = (self.0 ^ 0xff).wrapping_mul(0x100000001b3);
    }

    fn num(&mut self, n: i64) {
        self.bytes(&n.to_le_bytes());
    }
}

/// De dónde sale una fila y qué se sabe ya de su archivo.
struct Origin<'a> {
    dir_name: &'a str,
    unit: &'a Path,
    file: Option<PathBuf>,
    size: u64,
    kind: u8,
    hidden: bool,
    fp: u64,
    /// La sesión del escaneo anterior (si la hubo), por si ahora no se puede leer su conversación.
    old: Option<&'a SessionMeta>,
}

fn build(r: &SummaryRow, o: &Origin) -> SessionMeta {
    let raw = &r.raw[..];
    let mut m = SessionMeta {
        harness: Harness::Antigravity,
        id: r.id.clone(),
        file: o.file.clone().unwrap_or_else(|| o.unit.to_path_buf()),
        size: o.size,
        fingerprint: o.fp,
        hidden: o.hidden,
        ..SessionMeta::default()
    };

    // Carpeta de trabajo: la primera de `workspace_uris`; si falta, la del resumen.
    m.cwd = first_workspace(&r.uris).or_else(|| pb_str(raw, &[9, 1]).and_then(|u| uri_to_path(&u)));
    set_text(&mut m.ai_title, r.title.lines().find(|l| !l.trim().is_empty()).map(str::to_string));
    let preview = if r.preview.is_empty() { pb_str(raw, &[1]).unwrap_or_default() } else { r.preview.clone() };
    m.first_prompt = clean_prompt(unwrap_request(&preview));
    m.git_branch = pb_str(raw, &[9, 4]).map(|b| collapse(&b)).filter(|b| !b.is_empty() && b != "HEAD");
    m.started_ms = pb_secs(raw, &[7, 1]);
    // `CASCADE_RUN_STATUS_RUNNING` / `_BUSY`; por si algún día se guarda el número del enum (2 y 4).
    let status = r.status.trim().to_ascii_uppercase();
    m.busy = status.contains("RUNNING") || status.contains("BUSY") || status == "2" || status == "4";

    // Última actividad: la fila; si no, el resumen; si no, el archivo.
    m.mtime_ms = parse_time(&r.modified)
        .or_else(|| pb_secs(raw, &[3, 1]))
        .or_else(|| o.file.as_ref().and_then(|f| std::fs::metadata(f).ok()).map(|md| mtime_ms(&md)))
        .unwrap_or(0);

    // Lo que solo está en la conversación (solo `.db`, solo si la fila cambió).
    let mut source = r.source.trim().parse::<i64>().ok();
    let mut steps = r.steps;
    let mut entry_prev: Option<String> = None;
    if o.kind == 1
        && let Some(f) = &o.file
    {
        match read_conversation(f) {
            Some(c) => {
                source = c.source.or(source);
                steps = steps.max(c.steps);
                m.prompts = c.prompts;
                m.last_prompt = c.last_prompt;
                m.model = c.model;
            }
            // Puede ser un bloqueo pasajero: se reintenta en la siguiente pasada y, mientras tanto, se
            // conserva lo que ya se sabía (si no, los prompts y el modelo parpadearían a vacío).
            None => {
                m.fingerprint = 0;
                if let Some(old) = o.old {
                    m.prompts = old.prompts;
                    m.last_prompt = old.last_prompt.clone();
                    m.model = old.model.clone();
                    m.hidden |= old.hidden;
                    entry_prev = old.entrypoint.clone();
                }
            }
        }
    }
    // Un subagente que solo se distingue por su origen.
    if source == Some(16) {
        m.hidden = true;
    }
    m.entrypoint = source
        .and_then(entrypoint_of)
        .map(str::to_string)
        .or(entry_prev)
        .or_else(|| (o.dir_name == DATA_DIRS[0]).then(|| "cli".to_string()));
    m.has_messages = steps > 0 || !r.preview.is_empty() || !r.title.is_empty() || m.prompts > 0;
    m
}

/// Superficie según `trajectory_meta.source` (`CortexTrajectorySource`).
fn entrypoint_of(source: i64) -> Option<&'static str> {
    match source {
        17 => Some("cli"),
        1 | 21 => Some("desktop"),
        22 | 23 => Some("ide"),
        15 | 19 => Some("sdk"),
        _ => None,
    }
}

/// Lo que se lee del `.db` de una conversación.
struct Conversation {
    source: Option<i64>,
    steps: i64,
    prompts: u32,
    last_prompt: Option<String>,
    model: Option<String>,
}

/// Lee la conversación; `None` si no se puede abrir. Los pasos de tipo 14 son
/// los mensajes del usuario; su texto va en `step_payload` (campo 19.2).
fn read_conversation(path: &Path) -> Option<Conversation> {
    let conn = open_readonly(path).ok()?;
    // Abrir no basta: un archivo que no es SQLite solo falla al consultar.
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)).ok()?;
    let mut c = Conversation { source: None, steps: 0, prompts: 0, last_prompt: None, model: None };
    if has_table(&conn, "trajectory_meta") {
        c.source = conn.query_row("SELECT source FROM trajectory_meta LIMIT 1", [], |r| r.get::<_, i64>(0)).ok();
    }
    if has_table(&conn, "steps") {
        c.steps = conn.query_row("SELECT count(*) FROM steps", [], |r| r.get(0)).unwrap_or(0);
        c.prompts = conn.query_row("SELECT count(*) FROM steps WHERE step_type = 14", [], |r| r.get::<_, u32>(0)).unwrap_or(0);
        if c.prompts > 0 {
            let payload = conn
                .query_row("SELECT step_payload FROM steps WHERE step_type = 14 ORDER BY idx DESC LIMIT 1", [], |r| Ok(blob(r.get_ref(0)?)))
                .ok()
                .flatten();
            c.last_prompt = payload.as_deref().and_then(|p| pb_str(p, &[19, 2])).and_then(|t| clean_prompt(unwrap_request(&t)));
        }
    }
    c.model = last_blob(&conn, "executor_metadata").and_then(|b| pb_str(&b, &[10, 1, 28]));
    if c.model.is_none() {
        c.model = last_blob(&conn, "gen_metadata").and_then(|b| pb_str(&b, &[1, 19]));
    }
    c.model = c.model.map(|m| m.trim().to_string()).filter(|m| !m.is_empty() && m.len() <= 80 && m.chars().all(|ch| !ch.is_control()));
    Some(c)
}

fn blob(v: ValueRef) -> Option<Vec<u8>> {
    match v {
        ValueRef::Blob(b) => Some(b.to_vec()),
        _ => None,
    }
}

/// Campo `data` de la última fila de una tabla de metadatos.
fn last_blob(conn: &Connection, table: &str) -> Option<Vec<u8>> {
    if !has_table(conn, table) {
        return None;
    }
    conn.query_row(&format!("SELECT data FROM \"{table}\" ORDER BY idx DESC LIMIT 1"), [], |r| Ok(blob(r.get_ref(0)?))).ok().flatten()
}

/// El IDE envuelve el mensaje en `<USER_REQUEST>`: se queda con el interior.
fn unwrap_request(s: &str) -> &str {
    const OPEN: &str = "<USER_REQUEST>";
    match s.find(OPEN) {
        Some(i) => {
            let rest = &s[i + OPEN.len()..];
            &rest[..rest.find("</USER_REQUEST>").unwrap_or(rest.len())]
        }
        None => s,
    }
}

// ---------------------------------------------------------------- tiempo y rutas

/// Milisegundos de una fecha del índice (`2026-10-01 22:33:25.1234567+00:00`,
/// con o sin zona) o de un número (segundos o milisegundos). Las fechas
/// anteriores a 1970 (el marcador `0001-01-01`) no valen.
fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let ms = if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        let n: i64 = s.parse().ok()?;
        if n >= 100_000_000_000 { n } else { n.checked_mul(1000)? }
    } else {
        let iso = s.replacen(' ', "T", 1);
        match iso.parse::<jiff::Timestamp>() {
            Ok(t) => t.as_millisecond(),
            // Sin zona: se toma como UTC.
            Err(_) => iso.parse::<jiff::civil::DateTime>().ok()?.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp().as_millisecond(),
        }
    };
    (ms > 0).then_some(ms)
}

fn first_workspace(uris: &str) -> Option<String> {
    let v: Vec<serde_json::Value> = serde_json::from_str(uris).ok()?;
    // La primera que sea una carpeta local (puede haber URIs remotas delante).
    v.iter().filter_map(|u| u.as_str()).find_map(uri_to_path)
}

/// Decodifica `%XX`; un `%` mal formado se deja tal cual.
fn pct_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        (b as char).to_digit(16).map(|d| d as u8)
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_drive(s: &str) -> bool {
    s.len() >= 2 && s.as_bytes()[0].is_ascii_alphabetic() && s.as_bytes()[1] == b':'
}

/// Ruta local de un `file:` URI. Variantes: `file:///c:/x`, `file:///c%3A/x`,
/// `file://C:/x`, `file:///home/x` y UNC (`file://servidor/recurso/x`).
fn uri_to_path(uri: &str) -> Option<String> {
    let u = uri.trim();
    if !u.get(..5)?.eq_ignore_ascii_case("file:") {
        return None;
    }
    let rest = &u[5..];
    let (auth, path) = match rest.strip_prefix("//") {
        Some(r) => match r.find('/') {
            Some(i) => (&r[..i], &r[i..]),
            None => (r, ""),
        },
        None => ("", rest),
    };
    let auth = pct_decode(auth);
    let mut p = pct_decode(path);
    let unc = |s: &str| {
        let s = format!("\\\\{}", s.trim_start_matches('/').replace('/', "\\"));
        Some(s.trim_end_matches('\\').to_string()).filter(|s| s.len() > 2)
    };
    if auth.len() == 2 && is_drive(&auth) {
        // `file://C:/x`: la unidad cayó en el campo de servidor.
        p = format!("{auth}{p}");
    } else if !auth.is_empty() && !auth.eq_ignore_ascii_case("localhost") {
        return unc(&format!("{auth}{p}"));
    } else if p.starts_with("//") && !p.starts_with("///") {
        // `file:////servidor/recurso`
        return unc(&p);
    }
    let p = match p.strip_prefix('/') {
        Some(s) if is_drive(s) => s.to_string(),
        _ => p,
    };
    if is_drive(&p) {
        let mut s = p.replace('/', "\\");
        s[..1].make_ascii_uppercase();
        if s.len() > 3 {
            s = s.trim_end_matches('\\').to_string();
        }
        Some(s)
    } else if p.starts_with('/') {
        Some(if p.len() > 1 { p.trim_end_matches('/').to_string() } else { p })
    } else {
        None
    }
}

// ---------------------------------------------------------------- protobuf

/// Valor de un campo protobuf.
#[derive(Clone, Copy)]
enum Val<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

/// Recorre los campos de un mensaje sin esquema. Ante datos truncados o
/// malformados se detiene y conserva lo leído hasta ahí.
struct Fields<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Fields<'a> {
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..70).step_by(7) {
            let byte = *self.b.get(self.p)?;
            self.p += 1;
            if shift < 64 {
                v |= u64::from(byte & 0x7f) << shift;
            }
            if byte & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.p.checked_add(n).filter(|e| *e <= self.b.len())?;
        let s = &self.b[self.p..end];
        self.p = end;
        Some(s)
    }

    fn field(&mut self) -> Option<(u64, Val<'a>)> {
        let tag = self.varint()?;
        let num = tag >> 3;
        let val = match (num, tag & 7) {
            (0, _) => None,
            (_, 0) => self.varint().map(Val::Varint),
            (_, 1) => self.take(8).map(|_| Val::Varint(0)),
            (_, 5) => self.take(4).map(|_| Val::Varint(0)),
            (_, 2) => {
                let n = usize::try_from(self.varint()?).ok()?;
                self.take(n).map(Val::Bytes)
            }
            _ => None,
        };
        val.map(|v| (num, v))
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = (u64, Val<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.field();
        if item.is_none() {
            // Datos truncados o ajenos: no se sigue leyendo.
            self.p = self.b.len();
        }
        item
    }
}

/// Valor del campo al que lleva la ruta de números (`[9, 4]` = campo 4 del
/// mensaje del campo 9).
fn pb_get<'a>(b: &'a [u8], path: &[u64]) -> Option<Val<'a>> {
    let (first, rest) = path.split_first()?;
    for (num, v) in (Fields { b, p: 0 }) {
        if num != *first {
            continue;
        }
        if rest.is_empty() {
            return Some(v);
        }
        if let Val::Bytes(inner) = v
            && let Some(found) = pb_get(inner, rest)
        {
            return Some(found);
        }
    }
    None
}

/// Texto de un campo (recortado: son datos ajenos y pueden ser enormes).
fn pb_str(b: &[u8], path: &[u64]) -> Option<String> {
    match pb_get(b, path)? {
        Val::Bytes(s) => Some(String::from_utf8_lossy(&s[..s.len().min(4096)]).into_owned()).filter(|s| !s.trim().is_empty()),
        Val::Varint(_) => None,
    }
}

/// Segundos de un campo varint, en milisegundos (0 y valores absurdos no valen).
fn pb_secs(b: &[u8], path: &[u64]) -> Option<i64> {
    match pb_get(b, path)? {
        Val::Varint(s) if (100_000_000..100_000_000_000).contains(&s) => Some(s as i64 * 1000),
        _ => None,
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    // ------------------------------------------------------------ protobuf de prueba

    fn varint(mut n: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    fn f_num(num: u64, v: u64) -> Vec<u8> {
        let mut out = varint(num << 3);
        out.extend(varint(v));
        out
    }

    fn f_bytes(num: u64, b: &[u8]) -> Vec<u8> {
        let mut out = varint(num << 3 | 2);
        out.extend(varint(b.len() as u64));
        out.extend_from_slice(b);
        out
    }

    /// `raw_summary` con la forma real: preview, pasos, fechas y espacio de trabajo con rama.
    fn raw_summary(preview: &str, steps: u64, modified: u64, created: u64, uri: &str, branch: Option<&str>) -> Vec<u8> {
        let mut ws = f_bytes(1, uri.as_bytes());
        ws.extend(f_bytes(3, &[f_bytes(1, b"demo/repo"), f_bytes(2, b"https://example.invalid/demo/repo")].concat()));
        if let Some(b) = branch {
            ws.extend(f_bytes(4, b.as_bytes()));
        }
        [
            f_bytes(1, preview.as_bytes()),
            f_num(2, steps),
            f_bytes(3, &f_num(1, modified)),
            f_bytes(7, &f_num(1, created)),
            f_bytes(9, &ws),
        ]
        .concat()
    }

    fn payload(text: &str) -> Vec<u8> {
        [f_num(1, 7), f_bytes(19, &f_bytes(2, text.as_bytes()))].concat()
    }

    // ------------------------------------------------------------ fixtures

    const SUMMARIES_DDL: &str = "CREATE TABLE conversation_summaries (conversation_id text, title text NOT NULL DEFAULT '', \
        preview text NOT NULL DEFAULT '', step_count integer NOT NULL DEFAULT 0, last_modified_time datetime NOT NULL, \
        workspace_uris text NOT NULL, status text NOT NULL DEFAULT '', source text NOT NULL DEFAULT '', \
        project_id text NOT NULL DEFAULT '', agent_name text NOT NULL DEFAULT '', parent_conversation_id text NOT NULL DEFAULT '', \
        nesting_depth integer NOT NULL DEFAULT 0, battle_id text NOT NULL DEFAULT '', not_fully_idle numeric NOT NULL DEFAULT false, \
        killed numeric NOT NULL DEFAULT false, last_user_input_time datetime NOT NULL DEFAULT '', \
        last_user_input_step_index integer NOT NULL DEFAULT -1, app_data_dir text NOT NULL DEFAULT '', raw_summary blob, \
        group_id TEXT NOT NULL DEFAULT '', PRIMARY KEY (conversation_id));
        CREATE TABLE conversation_parents (conversation_id TEXT PRIMARY KEY, parent_id TEXT NOT NULL) WITHOUT ROWID;
        CREATE VIRTUAL TABLE conversations_fts USING fts5(conversation_id UNINDEXED, chunk_index UNINDEXED, archived UNINDEXED, title);";

    /// Fila de índice con valores razonables; cada prueba cambia lo que le interesa.
    struct Spec {
        id: String,
        title: String,
        preview: String,
        steps: i64,
        modified: String,
        uris: String,
        status: String,
        parent: String,
        nesting: i64,
        agent: String,
        app_dir: String,
        raw: Option<Vec<u8>>,
    }

    impl Spec {
        fn new(id: &str) -> Spec {
            Spec {
                id: id.into(),
                title: String::new(),
                preview: "Arregla el test del login".into(),
                steps: 42,
                modified: "2026-10-01 22:33:25.1234567+00:00".into(),
                uris: r#"["file:///c%3A/Users/demo/proyecto%20uno"]"#.into(),
                status: "CASCADE_RUN_STATUS_IDLE".into(),
                parent: String::new(),
                nesting: 0,
                agent: String::new(),
                app_dir: "antigravity-cli".into(),
                raw: None,
            }
        }
    }

    struct Fx {
        home: PathBuf,
    }

    impl Fx {
        fn new(name: &str) -> Fx {
            static N: AtomicU32 = AtomicU32::new(0);
            let home = std::env::temp_dir().join("claunch-tests").join(format!("agy-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home).unwrap();
            Fx { home }
        }

        fn dir(&self, name: &str) -> PathBuf {
            let d = self.home.join(name);
            std::fs::create_dir_all(d.join("conversations")).unwrap();
            d
        }

        fn unit(&self, name: &str) -> PathBuf {
            self.home.join(name).join(SUMMARIES)
        }

        fn summaries(&self, name: &str) -> Connection {
            let unit = self.dir(name).join(SUMMARIES);
            let c = Connection::open(unit).unwrap();
            let has: bool = c.query_row("SELECT count(*) FROM sqlite_master WHERE name = 'conversation_summaries'", [], |r| r.get::<_, i64>(0)).unwrap() > 0;
            if !has {
                c.execute_batch(SUMMARIES_DDL).unwrap();
            }
            c
        }

        fn run(&self, name: &str, prev: &Prev) -> Prev {
            index(&self.home, &self.unit(name), prev).into_iter().collect()
        }
    }

    impl Drop for Fx {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    fn put(c: &Connection, s: &Spec) {
        c.execute("DELETE FROM conversation_summaries WHERE conversation_id = ?1", [&s.id]).unwrap();
        c.execute(
            "INSERT INTO conversation_summaries (conversation_id, title, preview, step_count, last_modified_time, workspace_uris, status, \
             parent_conversation_id, nesting_depth, agent_name, app_data_dir, raw_summary) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            rusqlite::params![s.id, s.title, s.preview, s.steps, s.modified, s.uris, s.status, s.parent, s.nesting, s.agent, s.app_dir, s.raw],
        )
        .unwrap();
    }

    /// Crea `conversations/<id>.db` con pasos: (tipo, texto del prompt).
    fn conversation(dir: &Path, id: &str, source: i64, steps: &[(i64, &str)], model: Option<&str>) -> PathBuf {
        let path = dir.join("conversations").join(format!("{id}.db"));
        let _ = std::fs::remove_file(&path);
        let c = Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE trajectory_meta (trajectory_id text, cascade_id text, trajectory_type integer, source integer, PRIMARY KEY (trajectory_id));
             CREATE TABLE steps (idx integer, step_type integer NOT NULL DEFAULT 0, step_payload blob, PRIMARY KEY (idx));
             CREATE TABLE executor_metadata (idx integer, data blob, PRIMARY KEY (idx));
             CREATE TABLE gen_metadata (idx integer, data blob, PRIMARY KEY (idx));",
        )
        .unwrap();
        c.execute("INSERT INTO trajectory_meta VALUES ('t', ?1, 4, ?2)", rusqlite::params![id, source]).unwrap();
        for (i, (ty, text)) in steps.iter().enumerate() {
            c.execute("INSERT INTO steps (idx, step_type, step_payload) VALUES (?1, ?2, ?3)", rusqlite::params![i as i64, ty, payload(text)]).unwrap();
        }
        if let Some(m) = model {
            let data = f_bytes(10, &f_bytes(1, &f_bytes(28, m.as_bytes())));
            c.execute("INSERT INTO executor_metadata VALUES (0, ?1)", [data]).unwrap();
        }
        path
    }

    const ID1: &str = "11111111-2222-4333-8444-555555555555";
    const ID2: &str = "22222222-2222-4333-8444-555555555555";
    const ID3: &str = "33333333-2222-4333-8444-555555555555";

    fn get<'a>(p: &'a Prev, id: &str) -> &'a SessionMeta {
        p.get(&PathBuf::from(id)).unwrap_or_else(|| panic!("falta {id}"))
    }

    // ------------------------------------------------------------ descubrimiento

    #[test]
    fn units_are_the_existing_index_databases() {
        let fx = Fx::new("units");
        assert!(units(&fx.home).is_empty());
        drop(fx.summaries("antigravity"));
        assert_eq!(units(&fx.home), [fx.unit("antigravity")]);
        drop(fx.summaries("antigravity-cli"));
        assert_eq!(units(&fx.home), [fx.unit("antigravity-cli"), fx.unit("antigravity")]);
        // Una carpeta sin índice no cuenta.
        std::fs::create_dir_all(fx.home.join("antigravity-ide")).unwrap();
        assert_eq!(units(&fx.home).len(), 2);
    }

    #[test]
    fn indexes_a_cli_conversation_with_all_fields() {
        let fx = Fx::new("full");
        let dir = fx.dir("antigravity-cli");
        let c = fx.summaries("antigravity-cli");
        let mut s = Spec::new(ID1);
        let uri = "file:///c%3A/Users/demo/proyecto%20uno";
        s.raw = Some(raw_summary("Arregla el test del login", 42, 1_790_000_000, 1_789_990_000, uri, Some("feat/login")));
        put(&c, &s);
        let wrapped = "<USER_REQUEST>
Ahora el de registro
</USER_REQUEST>
<ADDITIONAL_METADATA>x</ADDITIONAL_METADATA>";
        let steps = [(14, "Arregla el test del login"), (15, "respuesta"), (14, wrapped), (132, "")];
        let file = conversation(&dir, ID1, 17, &steps, Some("gemini-3.8-flash-high"));

        let r = fx.run("antigravity-cli", &Prev::new());
        assert_eq!(r.len(), 1);
        let m = get(&r, ID1);
        assert_eq!(m.harness, Harness::Antigravity);
        assert_eq!(m.id, ID1);
        assert_eq!(m.file, file);
        assert!(m.size > 0);
        assert_eq!(m.cwd.as_deref(), Some("C:\\Users\\demo\\proyecto uno"));
        assert_eq!(m.first_prompt.as_deref(), Some("Arregla el test del login"));
        assert_eq!(m.last_prompt.as_deref(), Some("Ahora el de registro"));
        assert_eq!(m.prompts, 2);
        assert_eq!(m.git_branch.as_deref(), Some("feat/login"));
        assert_eq!(m.entrypoint.as_deref(), Some("cli"));
        assert_eq!(m.model.as_deref(), Some("gemini-3.8-flash-high"));
        assert_eq!(m.started_ms, Some(1_789_990_000_000));
        // La fecha de la fila (con décimas) manda sobre la del resumen.
        assert_eq!(m.mtime_ms, parse_time("2026-10-01 22:33:25.1234567+00:00").unwrap());
        assert_eq!(m.mtime_ms, 1_790_894_005_123);
        assert!(m.has_messages && !m.hidden && !m.busy);
        assert_eq!(Harness::Antigravity.index_key(m), PathBuf::from(ID1));
        assert_eq!(m.title(), "Arregla el test del login");
    }

    #[test]
    fn app_and_ide_conversations_get_their_surface_and_legacy_pb_is_never_read() {
        let fx = Fx::new("surfaces");
        let dir = fx.dir("antigravity");
        let c = fx.summaries("antigravity");
        let mut s1 = Spec::new(ID1);
        s1.app_dir = "antigravity".into();
        let mut s2 = Spec::new(ID2);
        s2.app_dir = String::new();
        let mut s3 = Spec::new(ID3);
        s3.app_dir = "antigravity".into();
        for s in [&s1, &s2, &s3] {
            put(&c, s);
        }
        conversation(&dir, ID1, 1, &[(14, "hola")], None);
        conversation(&dir, ID2, 22, &[(14, "hola")], None);
        // `.pb` con basura: si se abriera como SQLite o se leyera, no daría nada útil.
        std::fs::write(dir.join("conversations").join(format!("{ID3}.pb")), [0xde, 0xad, 0xbe, 0xef, 7, 7, 7]).unwrap();

        let r = fx.run("antigravity", &Prev::new());
        assert_eq!(get(&r, ID1).entrypoint.as_deref(), Some("desktop"));
        assert_eq!(get(&r, ID2).entrypoint.as_deref(), Some("ide"));
        let pb = get(&r, ID3);
        assert_eq!(pb.entrypoint, None);
        assert_eq!(pb.size, 7);
        assert!(pb.file.ends_with(format!("{ID3}.pb")));
        assert_eq!(pb.prompts, 0);
        assert!(pb.has_messages, "tiene pasos aunque no se puedan leer");
        assert_eq!(pb.first_prompt.as_deref(), Some("Arregla el test del login"));
    }

    #[test]
    fn without_conversation_file_the_index_database_is_the_file() {
        let fx = Fx::new("nofile");
        let c = fx.summaries("antigravity-cli");
        put(&c, &Spec::new(ID1));
        let r = fx.run("antigravity-cli", &Prev::new());
        let m = get(&r, ID1);
        assert_eq!(m.file, fx.unit("antigravity-cli"));
        assert_eq!(m.size, 0);
        assert_eq!(m.prompts, 0);
        assert_eq!(m.entrypoint.as_deref(), Some("cli"));
        assert!(m.has_messages);
    }

    // ------------------------------------------------------------ cwd

    #[test]
    fn uri_variants_become_paths() {
        let cases = [
            ("file:///c:/Users/demo/proyecto", "C:\\Users\\demo\\proyecto"),
            ("file:///c%3A/Users/demo/proyecto%20uno", "C:\\Users\\demo\\proyecto uno"),
            ("file:///C%3a/Users/demo", "C:\\Users\\demo"),
            ("file://C:/Users/demo/", "C:\\Users\\demo"),
            ("file:///g:/Shared%20drives/Equipo", "G:\\Shared drives\\Equipo"),
            ("file:///c:/", "C:\\"),
            ("file:///home/u/proj%20x", "/home/u/proj x"),
            ("file:///home/u/100%25done", "/home/u/100%done"),
            // `%` mal formado: se deja literal en vez de fallar.
            ("file:///home/u/100%done", "/home/u/100%done"),
            ("file:///home/u/50%", "/home/u/50%"),
            ("file:///home/u/%zz%4", "/home/u/%zz%4"),
            ("file:///home/u/ni%C3%B1o", "/home/u/niño"),
            ("file://localhost/home/u", "/home/u"),
            ("FILE:///home/u/", "/home/u"),
            ("file:///", "/"),
            ("file://servidor/recurso/p", "\\\\servidor\\recurso\\p"),
            ("file://server/share%20x/a/b/", "\\\\server\\share x\\a\\b"),
            ("file:////server/share/p", "\\\\server\\share\\p"),
        ];
        for (uri, want) in cases {
            assert_eq!(uri_to_path(uri).as_deref(), Some(want), "{uri}");
        }
        for bad in ["", "   ", "vscode-remote://ssh/x", "/home/u", "file:relativa", "file://", "http://x/y", "fi"] {
            assert_eq!(uri_to_path(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn cwd_comes_from_the_first_workspace_or_from_the_summary() {
        assert_eq!(first_workspace(r#"["file:///home/a", "file:///home/b"]"#).as_deref(), Some("/home/a"));
        assert_eq!(first_workspace(r#"[null, "file:///home/b"]"#).as_deref(), Some("/home/b"));
        for bad in ["", "[]", "no es json", r#"{"a":1}"#, r#"[1, 2]"#, r#"["vscode://x"]"#] {
            assert_eq!(first_workspace(bad), None, "{bad:?}");
        }
        let fx = Fx::new("cwd");
        let c = fx.summaries("antigravity-cli");
        let mut a = Spec::new(ID1);
        a.uris = String::new();
        a.raw = Some(raw_summary("p", 1, 1_790_000_000, 1_790_000_000, "file:///home/u/desde-resumen", None));
        let mut b = Spec::new(ID2);
        b.uris = "[]".into();
        let mut d = Spec::new(ID3);
        d.uris = "{no json".into();
        d.raw = Some(vec![0x0a, 0xff, 0xff, 0xff, 0xff, 0x0f, 1]);
        for s in [&a, &b, &d] {
            put(&c, s);
        }
        let r = fx.run("antigravity-cli", &Prev::new());
        assert_eq!(get(&r, ID1).cwd.as_deref(), Some("/home/u/desde-resumen"));
        // Sin carpeta de trabajo se indexa igual (el catálogo no la lista).
        assert_eq!(get(&r, ID2).cwd, None);
        assert_eq!(get(&r, ID3).cwd, None);
    }

    // ------------------------------------------------------------ exclusiones

    #[test]
    fn mirrors_placeholders_and_child_conversations_are_excluded_or_hidden() {
        let fx = Fx::new("excl");
        let dir = fx.dir("antigravity-cli");
        let c = fx.summaries("antigravity-cli");
        let ids: Vec<String> = (1..=9).map(|n| format!("{n}{n}111111-2222-4333-8444-555555555555")).collect();
        let [own, mirror, placeholder, child, nested, listed_child, with_agent, subagent, empty_dir] = &ids[..] else { panic!() };

        put(&c, &Spec::new(own));
        let mut s = Spec::new(mirror);
        s.app_dir = "antigravity".into();
        put(&c, &s);
        let mut s = Spec::new(placeholder);
        s.steps = 0;
        s.preview = String::new();
        s.modified = "0001-01-01 00:00:00+00:00".into();
        put(&c, &s);
        let mut s = Spec::new(child);
        s.parent = own.clone();
        put(&c, &s);
        let mut s = Spec::new(nested);
        s.nesting = 2;
        put(&c, &s);
        put(&c, &Spec::new(listed_child));
        c.execute("INSERT INTO conversation_parents VALUES (?1, ?2)", [listed_child, own]).unwrap();
        // Una sesión lanzada con `--agent` es del usuario: se lista.
        let mut s = Spec::new(with_agent);
        s.agent = "reviewer".into();
        put(&c, &s);
        put(&c, &Spec::new(subagent));
        conversation(&dir, subagent, 16, &[(14, "tarea del subagente")], None);
        // `app_data_dir` vacío = propia.
        let mut s = Spec::new(empty_dir);
        s.app_dir = String::new();
        put(&c, &s);

        let r = fx.run("antigravity-cli", &Prev::new());
        let visible = |id: &String| r.get(&PathBuf::from(id)).is_some_and(|m| !m.hidden && m.has_messages && m.cwd.is_some());
        assert!(visible(own));
        assert!(visible(with_agent));
        assert!(visible(empty_dir));
        assert!(!r.contains_key(&PathBuf::from(mirror)), "las filas espejo no se indexan");
        assert!(!r.contains_key(&PathBuf::from(placeholder)));
        for hidden in [child, nested, listed_child, subagent] {
            assert!(get(&r, hidden).hidden, "{hidden}");
        }
        assert_eq!(get(&r, with_agent).agent_name, None, "agent_name pisaría el título");
    }

    #[test]
    fn archived_battle_and_killed_conversations_are_hidden() {
        let fx = Fx::new("archived");
        let c = fx.summaries("antigravity-cli");
        for id in [ID1, ID2, ID3] {
            put(&c, &Spec::new(id));
        }
        c.execute("INSERT INTO conversations_fts (conversation_id, chunk_index, archived, title) VALUES (?1, -1, 1, 't')", [ID1]).unwrap();
        // Un trozo de contenido (chunk 0) con archived=1 no cuenta; solo la cabecera.
        c.execute("INSERT INTO conversations_fts (conversation_id, chunk_index, archived, title) VALUES (?1, 0, 1, 't')", [ID2]).unwrap();
        c.execute("INSERT INTO conversations_fts (conversation_id, chunk_index, archived, title) VALUES (?1, -1, 0, 't')", [ID3]).unwrap();
        let r = fx.run("antigravity-cli", &Prev::new());
        assert!(get(&r, ID1).hidden);
        assert!(!get(&r, ID2).hidden);
        assert!(!get(&r, ID3).hidden);

        // `battle_id` y `killed` ocultan sin tumbar nada.
        let fx = Fx::new("battle");
        let c = fx.summaries("antigravity-cli");
        put(&c, &Spec::new(ID1));
        put(&c, &Spec::new(ID2));
        c.execute("UPDATE conversation_summaries SET battle_id = 'b1' WHERE conversation_id = ?1", [ID1]).unwrap();
        c.execute("UPDATE conversation_summaries SET killed = 1 WHERE conversation_id = ?1", [ID2]).unwrap();
        let r = fx.run("antigravity-cli", &Prev::new());
        assert!(get(&r, ID1).hidden && get(&r, ID2).hidden);
    }

    // ------------------------------------------------------------ títulos, prompts, estado

    #[test]
    fn title_priority_and_first_line() {
        let fx = Fx::new("title");
        let c = fx.summaries("antigravity-cli");
        let mut a = Spec::new(ID1);
        a.title = "Migrar a Postgres\nsegunda línea".into();
        a.preview = "Prompt inicial".into();
        let b = Spec::new(ID2);
        let mut d = Spec::new(ID3);
        d.preview = String::new();
        for s in [&a, &b, &d] {
            put(&c, s);
        }
        let r = fx.run("antigravity-cli", &Prev::new());
        let m = get(&r, ID1);
        assert_eq!(m.ai_title.as_deref(), Some("Migrar a Postgres"));
        assert_eq!(m.custom_title, None);
        assert_eq!(m.title(), "Migrar a Postgres");
        assert_eq!(get(&r, ID2).title(), "Arregla el test del login");
        assert_eq!(get(&r, ID3).title(), "(sin título)");
        // La vista previa con el envoltorio del IDE se limpia.
        assert_eq!(unwrap_request("<USER_REQUEST>\nhaz esto\n</USER_REQUEST>\n<ADDITIONAL_METADATA>x"), "\nhaz esto\n");
        assert_eq!(unwrap_request("<USER_REQUEST>sin cierre"), "sin cierre");
        assert_eq!(unwrap_request("texto"), "texto");
    }

    #[test]
    fn prompt_count_counts_only_user_inputs() {
        let fx = Fx::new("prompts");
        let dir = fx.dir("antigravity-cli");
        let c = fx.summaries("antigravity-cli");
        put(&c, &Spec::new(ID1));
        put(&c, &Spec::new(ID2));
        conversation(&dir, ID1, 17, &[(14, "a"), (15, "b"), (101, "sistema"), (14, "c"), (14, "d"), (17, "error")], None);
        conversation(&dir, ID2, 17, &[(15, "solo respuestas"), (132, "")], None);
        let r = fx.run("antigravity-cli", &Prev::new());
        assert_eq!(get(&r, ID1).prompts, 3);
        assert_eq!(get(&r, ID1).last_prompt.as_deref(), Some("d"));
        assert_eq!(get(&r, ID2).prompts, 0);
        assert_eq!(get(&r, ID2).last_prompt, None);
    }

    #[test]
    fn model_falls_back_to_generation_metadata() {
        let fx = Fx::new("model");
        let dir = fx.dir("antigravity-cli");
        let c = fx.summaries("antigravity-cli");
        put(&c, &Spec::new(ID1));
        let path = conversation(&dir, ID1, 17, &[(14, "a")], None);
        {
            let rw = Connection::open(&path).unwrap();
            let data = f_bytes(1, &f_bytes(19, b"gemini-3.8-flash"));
            rw.execute("INSERT INTO gen_metadata VALUES (0, ?1)", [data]).unwrap();
        }
        let r = fx.run("antigravity-cli", &Prev::new());
        assert_eq!(get(&r, ID1).model.as_deref(), Some("gemini-3.8-flash"));
    }

    #[test]
    fn busy_follows_the_stored_status() {
        let fx = Fx::new("busy");
        let c = fx.summaries("antigravity-cli");
        for (id, st) in [(ID1, "CASCADE_RUN_STATUS_RUNNING"), (ID2, "CASCADE_RUN_STATUS_BUSY"), (ID3, "CASCADE_RUN_STATUS_IDLE")] {
            let mut s = Spec::new(id);
            s.status = st.into();
            put(&c, &s);
        }
        let r = fx.run("antigravity-cli", &Prev::new());
        assert!(get(&r, ID1).busy && get(&r, ID2).busy);
        assert!(!get(&r, ID3).busy);
    }

    // ------------------------------------------------------------ incremental

    #[test]
    fn unchanged_rows_are_reused_and_changed_rows_are_read_again() {
        let fx = Fx::new("incr");
        let dir = fx.dir("antigravity-cli");
        let c = fx.summaries("antigravity-cli");
        put(&c, &Spec::new(ID1));
        put(&c, &Spec::new(ID2));
        conversation(&dir, ID1, 17, &[(14, "uno")], None);
        conversation(&dir, ID2, 17, &[(14, "uno")], None);
        let r1 = fx.run("antigravity-cli", &Prev::new());
        assert_eq!(get(&r1, ID1).prompts, 1);

        // Si la fila no cambió, no se vuelve a abrir la conversación: el marcador sobrevive.
        let mut marked = r1.clone();
        marked.get_mut(&PathBuf::from(ID1)).unwrap().last_prompt = Some("MARCA".into());
        marked.get_mut(&PathBuf::from(ID2)).unwrap().last_prompt = Some("MARCA".into());
        let r2 = fx.run("antigravity-cli", &marked);
        assert_eq!(get(&r2, ID1).last_prompt.as_deref(), Some("MARCA"));

        // Llegan pasos nuevos y la fila cambia: solo esa se relee.
        conversation(&dir, ID1, 17, &[(14, "uno"), (15, "r"), (14, "dos")], None);
        let mut s = Spec::new(ID1);
        s.steps = 43;
        s.modified = "2026-10-02 08:00:00.5000000+00:00".into();
        put(&c, &s);
        let r3 = fx.run("antigravity-cli", &r2);
        assert_eq!(get(&r3, ID1).prompts, 2);
        assert_eq!(get(&r3, ID1).last_prompt.as_deref(), Some("dos"));
        assert_eq!(get(&r3, ID2).last_prompt.as_deref(), Some("MARCA"));

        // Incremental == completo.
        let full = fx.run("antigravity-cli", &Prev::new());
        let mut a: Vec<_> = r3.into_iter().collect();
        let mut b: Vec<_> = full.into_iter().collect();
        a.sort_by(|x, y| x.0.cmp(&y.0));
        b.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(a.len(), 2);
        let (ma, mb) = (&a[0].1, &b[0].1);
        assert_eq!(ma.prompts, mb.prompts);
        assert_eq!(ma.last_prompt, mb.last_prompt);
        assert_eq!(ma.mtime_ms, mb.mtime_ms);
        assert_eq!(ma.fingerprint, mb.fingerprint);
        // (el segundo conserva la marca a propósito; salvo eso, todo coincide)
        let mut m2 = a[1].1.clone();
        m2.last_prompt = b[1].1.last_prompt.clone();
        assert_eq!(m2, b[1].1);
    }

    #[test]
    fn file_kind_changes_and_deleted_rows_are_followed() {
        let fx = Fx::new("kind");
        let dir = fx.dir("antigravity-cli");
        let c = fx.summaries("antigravity-cli");
        put(&c, &Spec::new(ID1));
        put(&c, &Spec::new(ID2));
        let r1 = fx.run("antigravity-cli", &Prev::new());
        assert_eq!(get(&r1, ID1).prompts, 0);
        // Aparece el archivo de la conversación sin que la fila cambie.
        conversation(&dir, ID1, 17, &[(14, "a"), (14, "b")], Some("m1"));
        c.execute("DELETE FROM conversation_summaries WHERE conversation_id = ?1", [ID2]).unwrap();
        let r2 = fx.run("antigravity-cli", &r1);
        assert_eq!(get(&r2, ID1).prompts, 2);
        assert!(get(&r2, ID1).size > 0);
        assert!(!r2.contains_key(&PathBuf::from(ID2)), "una fila borrada desaparece");
    }

    // ------------------------------------------------------------ entradas rotas

    #[test]
    fn broken_inputs_do_not_abort_the_listing() {
        let fx = Fx::new("broken");
        let dir = fx.dir("antigravity-cli");
        let c = fx.summaries("antigravity-cli");
        // `raw_summary` NULL, truncado, y valores sin sentido.
        let mut a = Spec::new(ID1);
        a.raw = None;
        let mut b = Spec::new(ID2);
        let full = raw_summary("p", 3, 1_790_000_000, 1_789_000_000, "file:///home/u/x", Some("main"));
        b.raw = Some(full[..full.len() - 9].to_vec());
        let mut d = Spec::new(ID3);
        d.modified = "no es una fecha".into();
        d.raw = Some(vec![0xff; 40]);
        for s in [&a, &b, &d] {
            put(&c, s);
        }
        // Conversaciones ilegibles: basura en lugar de SQLite.
        std::fs::write(dir.join("conversations").join(format!("{ID1}.db")), b"esto no es sqlite, ni de lejos").unwrap();
        let r = fx.run("antigravity-cli", &Prev::new());
        assert_eq!(r.len(), 3);
        let m = get(&r, ID1);
        assert!(m.has_messages && m.cwd.is_some());
        assert_eq!(m.prompts, 0);
        assert_eq!(m.fingerprint, 0, "una conversación ilegible se reintenta");
        assert!(get(&r, ID2).started_ms.is_some());
        assert_eq!(get(&r, ID2).git_branch, None, "el campo cortado no existe");
        let m = get(&r, ID3);
        assert_eq!(m.mtime_ms, 0);
        assert_eq!(m.started_ms, None);
        assert_eq!(m.git_branch, None);
    }

    #[test]
    fn unreadable_index_keeps_the_previous_sessions_of_that_folder() {
        let fx = Fx::new("keep");
        let c = fx.summaries("antigravity-cli");
        put(&c, &Spec::new(ID1));
        drop(c);
        let c2 = fx.summaries("antigravity");
        let mut s = Spec::new(ID2);
        s.app_dir = "antigravity".into();
        put(&c2, &s);
        drop(c2);
        let mut prev = fx.run("antigravity-cli", &Prev::new());
        prev.extend(fx.run("antigravity", &Prev::new()));
        assert_eq!(prev.len(), 2);

        std::fs::write(fx.unit("antigravity-cli"), b"basura que no es una base de datos").unwrap();
        let kept = fx.run("antigravity-cli", &prev);
        assert_eq!(kept.len(), 1, "solo las de esa carpeta");
        assert_eq!(kept[&PathBuf::from(ID1)], prev[&PathBuf::from(ID1)]);
        // La otra base sigue funcionando.
        assert_eq!(fx.run("antigravity", &prev).len(), 1);

        // Una base que desapareció tampoco borra lo conocido.
        std::fs::remove_file(fx.unit("antigravity-cli")).unwrap();
        assert_eq!(fx.run("antigravity-cli", &prev).len(), 1);
    }

    #[test]
    fn tolerates_other_schemas() {
        let fx = Fx::new("schema");
        fx.dir("antigravity-cli");
        let c = Connection::open(fx.unit("antigravity-cli")).unwrap();
        // Esquema antiguo: faltan casi todas las columnas, las tablas auxiliares y el FTS.
        c.execute_batch(
            "CREATE TABLE conversation_summaries (conversation_id text PRIMARY KEY, preview text, step_count integer, last_modified_time text, workspace_uris text);
             INSERT INTO conversation_summaries VALUES ('abc-1', 'Hola', 3, '2026-10-01 10:00:00', '[\"file:///home/u/p\"]');
             INSERT INTO conversation_summaries VALUES ('../../fuera', 'Hola', 3, 1790000000, '[\"file:///home/u/q\"]');",
        )
        .unwrap();
        drop(c);
        let r = fx.run("antigravity-cli", &Prev::new());
        assert_eq!(r.len(), 2);
        let m = get(&r, "abc-1");
        assert_eq!(m.cwd.as_deref(), Some("/home/u/p"));
        assert_eq!(m.mtime_ms, parse_time("2026-10-01 10:00:00").unwrap());
        assert_eq!(get(&r, "../../fuera").mtime_ms, 1_790_000_000_000);
        // Un id con separadores nunca se convierte en ruta de archivo.
        assert_eq!(get(&r, "../../fuera").file, fx.unit("antigravity-cli"));

        // Sin tabla de resúmenes: se avisa y no hay sesiones.
        let fx = Fx::new("schema2");
        fx.dir("antigravity-cli");
        Connection::open(fx.unit("antigravity-cli")).unwrap().execute_batch("CREATE TABLE otra (x)").unwrap();
        assert!(fx.run("antigravity-cli", &Prev::new()).is_empty());
    }

    // ------------------------------------------------------------ utilidades

    #[test]
    fn protobuf_reader_survives_truncation_and_garbage() {
        let msg = [f_bytes(1, b"hola"), f_bytes(9, &[f_bytes(1, b"uri"), f_bytes(4, b"rama")].concat()), f_num(2, 42)].concat();
        assert_eq!(pb_str(&msg, &[1]).as_deref(), Some("hola"));
        assert_eq!(pb_str(&msg, &[9, 4]).as_deref(), Some("rama"));
        assert_eq!(pb_str(&msg, &[9, 1]).as_deref(), Some("uri"));
        assert!(matches!(pb_get(&msg, &[2]), Some(Val::Varint(42))));
        assert!(pb_get(&msg, &[3]).is_none());
        assert!(pb_get(&msg, &[]).is_none());
        // Truncado: lo anterior se conserva y lo cortado no existe.
        for cut in 0..msg.len() {
            let _ = pb_get(&msg[..cut], &[9, 4]);
        }
        assert_eq!(pb_str(&msg[..8], &[1]).as_deref(), Some("hola"));
        assert!(pb_get(&msg[..14], &[9, 4]).is_none());
        // Basura: longitudes enormes, varints interminables, campo 0, tipos de grupo.
        for junk in [&[0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01][..], &[0x80; 30][..], &[0x00, 0x01][..], &[0x0b, 0x0c][..], &[][..]] {
            assert!(pb_get(junk, &[1]).is_none());
        }
        // Campos repetidos: sigue buscando hasta dar con la ruta completa.
        let rep = [f_bytes(9, &f_bytes(1, b"a")), f_bytes(9, &f_bytes(4, b"b"))].concat();
        assert_eq!(pb_str(&rep, &[9, 4]).as_deref(), Some("b"));
        // Fechas absurdas no valen.
        assert_eq!(pb_secs(&f_bytes(7, &f_num(1, 5)), &[7, 1]), None);
        assert_eq!(pb_secs(&f_bytes(7, &f_num(1, 1_790_000_000)), &[7, 1]), Some(1_790_000_000_000));
    }

    #[test]
    fn parses_index_dates() {
        assert_eq!(parse_time("2026-10-01 22:33:25.1234567+00:00"), Some(1_790_894_005_123));
        assert_eq!(parse_time("2026-10-01T22:33:25Z"), Some(1_790_894_005_000));
        assert_eq!(parse_time("2026-10-01 22:33:25"), Some(1_790_894_005_000));
        assert_eq!(parse_time("2026-10-02 00:33:25+02:00"), Some(1_790_894_005_000));
        assert_eq!(parse_time("1790894005"), Some(1_790_894_005_000));
        assert_eq!(parse_time("1790894005123"), Some(1_790_894_005_123));
        for bad in ["", "ayer", "0001-01-01 00:00:00+00:00", "0", "99999999999999999999"] {
            assert_eq!(parse_time(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn huge_prompt_text_is_bounded() {
        let big = "a ".repeat(3_000_000);
        let p = payload(&big);
        let t = pb_str(&p, &[19, 2]).unwrap();
        assert!(t.len() <= 4096);
        assert!(clean_prompt(&t).unwrap().chars().count() <= 285);
    }

    // ------------------------------------------------------------ argumentos

    fn opts(model: Option<&str>, perm: Option<&str>) -> LaunchOptions {
        LaunchOptions {
            model: model.map(str::to_string),
            permission_mode: perm.map(str::to_string),
            name: Some("ignorado".into()),
            worktree: Some("ignorado".into()),
            fork: true,
            ..LaunchOptions::default()
        }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn args_for_new_resume_and_fork() {
        assert_eq!(args(&Mode::New, &LaunchOptions::default()), (Vec::<String>::new(), None));
        assert_eq!(args(&Mode::Resume("abc".into()), &LaunchOptions::default()), (s(&["--conversation", "abc"]), Some("abc".into())));
        // No hay fork: se reanuda (y el nombre y el worktree se ignoran).
        assert_eq!(args(&Mode::Fork("abc".into()), &opts(None, None)), (s(&["--conversation", "abc"]), Some("abc".into())));
        assert_eq!(args(&Mode::New, &opts(None, None)).0, Vec::<String>::new());
    }

    #[test]
    fn args_translate_model_and_permissions() {
        let (a, _) = args(&Mode::New, &opts(Some("claude-sonnet-4-6"), Some("plan")));
        assert_eq!(a, s(&["--model", "claude-sonnet-4-6", "--mode", "plan"]));
        let (a, id) = args(&Mode::Resume("x".into()), &opts(Some(" gemini-3.8-flash-low "), Some("accept-edits")));
        assert_eq!(a, s(&["--conversation", "x", "--model", "gemini-3.8-flash-low", "--mode", "accept-edits"]));
        assert_eq!(id.as_deref(), Some("x"));
        assert_eq!(args(&Mode::New, &opts(None, Some("auto"))).0, s(&["--mode", "auto"]));
        assert_eq!(args(&Mode::New, &opts(None, Some("bypass"))).0, s(&["--dangerously-skip-permissions"]));
        // Vacío o desconocido: nada.
        for p in ["", "acceptEdits", "bypassPermissions", "otra"] {
            assert!(args(&Mode::New, &opts(Some(""), Some(p))).0.is_empty(), "{p}");
        }
        // Cada modo del Info se traduce a algo (salvo el primero).
        for perm in INFO.permissions.iter().skip(1) {
            assert!(!args(&Mode::New, &opts(None, Some(perm.value))).0.is_empty(), "{}", perm.value);
        }
    }

    #[test]
    fn home_is_the_common_gemini_folder() {
        assert_eq!(home().file_name().and_then(|n| n.to_str()), Some(".gemini"));
    }

    // ------------------------------------------------------------ revisión adversarial

    #[test]
    fn a_second_pass_without_changes_returns_exactly_the_same() {
        let fx = Fx::new("determinism");
        let dir = fx.dir("antigravity-cli");
        let c = fx.summaries("antigravity-cli");
        for id in [ID1, ID2, ID3] {
            put(&c, &Spec::new(id));
        }
        conversation(&dir, ID1, 17, &[(14, "uno"), (15, "r"), (14, "dos")], Some("m1"));
        // Conversación ilegible (huella 0, se reintenta) y subagente.
        std::fs::write(dir.join("conversations").join(format!("{ID2}.db")), b"basura").unwrap();
        conversation(&dir, ID3, 16, &[(14, "sub")], None);
        let r1 = fx.run("antigravity-cli", &Prev::new());
        let r2 = fx.run("antigravity-cli", &r1);
        let r3 = fx.run("antigravity-cli", &r2);
        assert_eq!(r1, r2, "si no, el catálogo se republicaría cada 3 s");
        assert_eq!(r2, r3);
        assert_eq!(get(&r2, ID2).fingerprint, 0);
        assert!(get(&r2, ID3).hidden);
    }

    #[test]
    fn a_conversation_that_cannot_be_read_now_keeps_what_was_known() {
        let fx = Fx::new("transient");
        let dir = fx.dir("antigravity-cli");
        let c = fx.summaries("antigravity-cli");
        put(&c, &Spec::new(ID1));
        put(&c, &Spec::new(ID2));
        conversation(&dir, ID1, 17, &[(14, "uno"), (14, "dos")], Some("m1"));
        conversation(&dir, ID2, 16, &[(14, "sub")], None);
        let r1 = fx.run("antigravity-cli", &Prev::new());
        assert_eq!(get(&r1, ID1).prompts, 2);
        assert!(get(&r1, ID2).hidden);
        // La fila cambia y justo entonces el archivo no se puede abrir.
        for id in [ID1, ID2] {
            let mut s = Spec::new(id);
            s.modified = "2026-10-03 08:00:00+00:00".into();
            put(&c, &s);
            std::fs::write(dir.join("conversations").join(format!("{id}.db")), b"bloqueado").unwrap();
        }
        let r2 = fx.run("antigravity-cli", &r1);
        let m = get(&r2, ID1);
        assert_eq!((m.prompts, m.last_prompt.as_deref(), m.model.as_deref()), (2, Some("dos"), Some("m1")));
        assert_eq!(m.entrypoint.as_deref(), Some("cli"));
        assert_eq!(m.fingerprint, 0);
        assert!(get(&r2, ID2).hidden, "un subagente no aparece en la lista por un fallo pasajero");
        assert_eq!(fx.run("antigravity-cli", &r2), r2);
    }

    #[test]
    fn huge_fields_and_odd_rows_do_not_break_the_index() {
        let fx = Fx::new("huge");
        let c = fx.summaries("antigravity-cli");
        let mut a = Spec::new(ID1);
        a.title = "t ".repeat(2_000_000);
        a.preview = format!("<USER_REQUEST>{}</USER_REQUEST>", "p ".repeat(2_000_000));
        a.raw = Some(vec![0x0a; 3_000_000]);
        put(&c, &a);
        let mut b = Spec::new(ID2);
        b.uris = format!("[\"file:///home/u/{}\"]", "x".repeat(100_000));
        put(&c, &b);
        // Tipos inesperados en las columnas: números en lugar de texto, texto donde va un número.
        let odd = "UPDATE conversation_summaries SET step_count = 'doce', status = 4, nesting_depth = 'x' WHERE conversation_id = ?1";
        c.execute(odd, [ID2]).unwrap();
        let r = fx.run("antigravity-cli", &Prev::new());
        assert_eq!(r.len(), 2);
        let m = get(&r, ID1);
        assert!(m.ai_title.as_deref().is_some_and(|t| t.chars().count() <= 285));
        assert!(m.first_prompt.as_deref().is_some_and(|t| t.chars().count() <= 285));
        let m = get(&r, ID2);
        assert!(m.busy, "el estado numérico 4 es BUSY");
        assert!(m.has_messages && m.cwd.is_some());
        assert_eq!(fx.run("antigravity-cli", &r), r);
    }

    #[test]
    fn empty_missing_or_odd_databases_never_panic() {
        let fx = Fx::new("empty");
        fx.dir("antigravity-cli");
        // Archivo de 0 bytes (SQLite lo ve como una base vacía): sin esquema, sin sesiones.
        std::fs::write(fx.unit("antigravity-cli"), b"").unwrap();
        assert!(fx.run("antigravity-cli", &Prev::new()).is_empty());
        // Tabla sin filas.
        let fx = Fx::new("empty2");
        drop(fx.summaries("antigravity-cli"));
        assert!(fx.run("antigravity-cli", &Prev::new()).is_empty());
        // Base inexistente.
        assert!(index(&fx.home, &fx.home.join("nada").join(SUMMARIES), &Prev::new()).is_empty());
        // La tabla existe pero sin la columna del id.
        let fx = Fx::new("empty3");
        fx.dir("antigravity-cli");
        Connection::open(fx.unit("antigravity-cli")).unwrap().execute_batch("CREATE TABLE conversation_summaries (x)").unwrap();
        assert!(fx.run("antigravity-cli", &Prev::new()).is_empty());
        // Un índice cuyas tablas auxiliares están rotas (FTS sin las columnas esperadas) sigue listando.
        let fx = Fx::new("empty4");
        fx.dir("antigravity-cli");
        let c = Connection::open(fx.unit("antigravity-cli")).unwrap();
        c.execute_batch(
            "CREATE TABLE conversation_summaries (conversation_id text PRIMARY KEY, preview text, workspace_uris text);
             INSERT INTO conversation_summaries VALUES ('x1', 'hola', '[\"file:///home/u/p\"]');
             CREATE TABLE conversations_fts (otra text); CREATE TABLE conversation_parents (otra text);",
        )
        .unwrap();
        drop(c);
        let r = fx.run("antigravity-cli", &Prev::new());
        assert_eq!(r.len(), 1);
        assert!(get(&r, "x1").has_messages && !get(&r, "x1").hidden);
    }

    #[test]
    fn conversation_databases_with_other_schemas_still_give_what_they_have() {
        let fx = Fx::new("convschema");
        let dir = fx.dir("antigravity-cli");
        let c = fx.summaries("antigravity-cli");
        for id in [ID1, ID2, ID3] {
            put(&c, &Spec::new(id));
        }
        let make = |id: &str, ddl: &str| {
            let rw = Connection::open(dir.join("conversations").join(format!("{id}.db"))).unwrap();
            rw.execute_batch(ddl).unwrap();
        };
        // Sin `trajectory_meta` ni metadatos: solo pasos.
        make(ID1, "CREATE TABLE steps (idx integer PRIMARY KEY, step_type integer); INSERT INTO steps VALUES (0, 14), (1, 14), (2, 15);");
        // Sin `step_payload`: se cuentan los prompts pero no hay texto.
        make(ID2, "CREATE TABLE steps (idx integer PRIMARY KEY, step_type integer, otra blob); INSERT INTO steps VALUES (0, 14, x'00');");
        // Sin pasos ni nada.
        make(ID3, "CREATE TABLE otra (x)");
        let r = fx.run("antigravity-cli", &Prev::new());
        assert_eq!(get(&r, ID1).prompts, 2);
        assert_eq!(get(&r, ID1).last_prompt, None);
        assert_eq!(get(&r, ID2).prompts, 1);
        assert_eq!(get(&r, ID3).prompts, 0);
        for id in [ID1, ID2, ID3] {
            assert!(get(&r, id).fingerprint > 0, "una base legible no se reintenta");
            assert!(get(&r, id).has_messages);
        }
    }

    #[test]
    fn a_remote_workspace_before_a_local_one_does_not_hide_the_session() {
        assert_eq!(first_workspace(r#"["vscode-remote://ssh-remote+x/home/u", "file:///home/u/p"]"#).as_deref(), Some("/home/u/p"));
        assert_eq!(uri_to_path("file:///home/u/~tilde/"), Some("/home/u/~tilde".into()));
        assert_eq!(uri_to_path("file:///home/u/a%20b%2Fc"), Some("/home/u/a b/c".into()));
    }

    #[test]
    fn the_same_id_in_both_databases_is_indexed_per_folder() {
        let fx = Fx::new("dup");
        for name in ["antigravity-cli", "antigravity"] {
            let c = fx.summaries(name);
            let mut s = Spec::new(ID1);
            s.app_dir = String::new();
            put(&c, &s);
        }
        let a = fx.run("antigravity-cli", &Prev::new());
        let b = fx.run("antigravity", &Prev::new());
        assert_eq!((a.len(), b.len()), (1, 1));
        assert_eq!(get(&a, ID1).entrypoint.as_deref(), Some("cli"));
        assert_eq!(get(&b, ID1).entrypoint, None);
        // Con las dos mezcladas en `prev`, cada carpeta reutiliza solo lo suyo.
        let mut both = a.clone();
        both.extend(b.clone());
        assert_eq!(fx.run("antigravity", &both), b);
    }

    #[test]
    fn mode_is_dropped_only_for_an_agy_that_lacks_it() {
        let old = "Usage: agy [flags]\n  --add-dir  -c, --continue  --conversation string  --dangerously-skip-permissions  --model string  --sandbox";
        let new = "Usage: agy [flags]\n  --conversation string  --mode string   (accept-edits, auto, plan)  --model string";
        let args = || vec!["--conversation".to_string(), "abc".into(), "--mode".into(), "plan".into(), "--model".into(), "m".into()];

        let mut a = args();
        let note = drop_mode(&mut a, old).expect("el agy antiguo no admite --mode");
        assert_eq!(a, ["--conversation", "abc", "--model", "m"]);
        assert!(note.contains("«plan»"), "{note}");
        // El actual lo admite; y una ayuda que no es la de agy no decide nada.
        for help in [new, "", "wsl: uso …"] {
            let mut a = args();
            assert_eq!(drop_mode(&mut a, help), None);
            assert_eq!(a, args());
        }
        // `--mode` al final, sin valor: se quita sin pasarse del final.
        let mut a = vec!["--mode".to_string()];
        assert!(drop_mode(&mut a, old).is_some());
        assert!(a.is_empty());

        // Sin `--mode`, o con el ejecutable de su actualizador, ni se pregunta:
        // la carpeta para la salida de `--help` ni se crea.
        let scratch = std::env::temp_dir().join("claunch-tests").join(format!("agy-preflight-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        let mut a = vec!["--model".to_string(), "m".into()];
        assert_eq!(preflight_in(Path::new("claunch-no-existe-agy"), &mut a, &scratch), None);
        let mut a = args();
        assert_eq!(preflight_in(&home().join("bin").join("agy"), &mut a, &scratch), None);
        assert!(!scratch.exists());
        // Un programa que no arranca no decide nada, y no deja restos.
        assert_eq!(preflight_in(Path::new("claunch-no-existe-agy"), &mut a, &scratch), None);
        assert_eq!(a, args());
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
        // Sin carpeta donde escribir (aquí, un archivo en su lugar) tampoco.
        let file = scratch.join("ocupado");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(preflight_in(Path::new("claunch-no-existe-agy"), &mut a, &file), None);
        assert_eq!(a, args());
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn help_is_read_from_the_program_or_given_up() {
        let scratch = std::env::temp_dir().join("claunch-tests").join(format!("agy-help-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        assert_eq!(help_of(Path::new("claunch-no-existe-agy"), &scratch), None);
        // Un programa real que entiende `--help`: este mismo binario de tests.
        let me = std::env::current_exe().unwrap();
        let help = help_of(&me, &scratch).expect("la ayuda del binario de tests");
        assert!(help.contains("--test-threads"), "{help}");
        // No queda nada en la carpeta.
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn warnings_never_carry_conversation_content() {
        // Los avisos solo llevan la ruta de la base y una frase fija.
        let src = include_str!("antigravity.rs");
        let body = &src[..src.find("#[cfg(test)]").unwrap_or(src.len())];
        for line in body.lines().filter(|l| l.contains("warn_log!")) {
            assert!(line.contains("unit.display()"), "{line}");
        }
    }
}
