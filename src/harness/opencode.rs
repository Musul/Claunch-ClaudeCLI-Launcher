//! opencode 2.x: sesiones en la base SQLite `~/.local/share/opencode/opencode.db`.
//!
//! Todo vive en una única base (modo WAL) que opencode escribe mientras claunch
//! lee, así que se abre en solo lectura y con consultas cortas. Cada sesión es
//! una fila de `session_v2`; los prompts humanos son las filas `user` de
//! `session_message`. La columna `data` de los mensajes puede pesar cientos de
//! KB: nunca se trae entera, solo `json_extract` del texto.
//!
//! Incremental: la huella de cada fila (tiempos + título + carpeta + modelo +
//! nº y último `seq` de prompts) se guarda en `SessionMeta::fingerprint`; si no
//! cambió, se reutiliza el `SessionMeta` anterior sin leer textos. Los prompts
//! entran en la huella porque no está verificado que `time_updated` suba con
//! cada mensaje: así un prompt nuevo nunca queda sin verse.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, Row};

use super::{Info, ModelInput, Perm, Prev, sqlite};
use crate::config::LaunchOptions;
use crate::data::SessionMeta;
use crate::data::sessions::collapse;
use crate::harness::Harness;
use crate::launch::Mode;

/// Solo lo verificado contra `--help` de v2.2.5: la TUI raíz no tiene `--fork`,
/// `--model`, nombre ni worktree (esos flags son de `run`/`mini`).
pub const INFO: Info = Info {
    id: "opencode",
    label: "opencode",
    tag: "opencode",
    program: "opencode",
    install: "npm install -g @opencode/cli",
    color: "#E28FB5",
    fork: false,
    name: false,
    worktree: false,
    model: ModelInput::None,
    permissions: &[
        Perm { value: "", label: "por defecto", hint: "" },
        Perm { value: "auto", label: "auto", hint: "aprueba los permisos que no estén denegados (--auto)" },
    ],
};

/// Máximo de filas `user` que se miran buscando un texto legible (primer/último
/// prompt) antes de rendirse: evita recorrer miles de filas ilegibles.
const MAX_PROMPT_ROWS: usize = 64;

/// Avisos de «una sola vez» (la base ilegible o con otro esquema se reintenta cada 3 s).
static WARNED_OPEN: AtomicBool = AtomicBool::new(false);
static WARNED_SCHEMA: AtomicBool = AtomicBool::new(false);

fn warn_once(flag: &AtomicBool, msg: &str) {
    if !flag.swap(true, Ordering::Relaxed) {
        crate::warn_log!("{msg}");
    }
}

/// Carpeta de datos (`XDG_DATA_HOME/opencode` o `~/.local/share/opencode`, también en Windows).
pub fn home() -> PathBuf {
    match std::env::var_os("XDG_DATA_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("opencode"),
        _ => crate::paths::home().join(".local").join("share").join("opencode"),
    }
}

/// Ruta de la base: `OPENCODE_DB` (absoluta o relativa a la carpeta de datos;
/// `:memory:` no es un archivo y se ignora) o `opencode.db`.
fn db_path(home: &Path, env_db: Option<&str>) -> PathBuf {
    match env_db.map(str::trim) {
        Some(v) if !v.is_empty() && v != ":memory:" => {
            let p = Path::new(v);
            if p.is_absolute() { p.to_path_buf() } else { home.join(p) }
        }
        _ => home.join("opencode.db"),
    }
}

pub fn units(home: &Path) -> Vec<PathBuf> {
    units_in(home, std::env::var("OPENCODE_DB").ok().as_deref())
}

/// La base de la carpeta de datos, si existe (`env_db`: valor de `OPENCODE_DB`).
fn units_in(home: &Path, env_db: Option<&str>) -> Vec<PathBuf> {
    let db = db_path(home, env_db);
    if db.is_file() { vec![db] } else { Vec::new() }
}

/// Todas las sesiones de la base. Si no se puede leer, las del escaneo
/// anterior: mejor datos viejos que una lista vacía.
pub fn index(_home: &Path, unit: &Path, prev: &Prev) -> Vec<(PathBuf, SessionMeta)> {
    match read_all(unit, prev) {
        Ok(found) => found,
        Err(e) => {
            // `e` es de SQLite (sin contenido de mensajes).
            warn_once(&WARNED_OPEN, &format!("opencode: no se pudo leer {}: {e}", unit.display()));
            prev.iter().map(|(k, m)| (k.clone(), m.clone())).collect()
        }
    }
}

/// Fila de `session_v2` con lo que hace falta para decidir y para la huella.
struct SessionRow {
    id: String,
    directory: String,
    title: Option<String>,
    slug: Option<String>,
    model: Option<String>,
    created: Option<i64>,
    updated: Option<i64>,
    idle: Option<i64>,
    busy: Option<i64>,
    archived: Option<i64>,
}

fn int(r: &Row, i: usize) -> Option<i64> {
    match r.get_ref(i).ok()? {
        ValueRef::Integer(v) => Some(v),
        ValueRef::Real(f) => Some(f as i64),
        ValueRef::Text(t) => std::str::from_utf8(t).ok()?.trim().parse().ok(),
        _ => None,
    }
}

/// `time_archived`: cualquier valor no nulo cuenta como archivada, aunque no sea un número legible.
fn archived(r: &Row, i: usize) -> Option<i64> {
    match r.get_ref(i).ok()? {
        ValueRef::Null => None,
        _ => Some(int(r, i).unwrap_or(-1)),
    }
}

fn text(r: &Row, i: usize) -> Option<String> {
    match r.get_ref(i).ok()? {
        ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
        _ => None,
    }
}

fn read_all(unit: &Path, prev: &Prev) -> rusqlite::Result<Vec<(PathBuf, SessionMeta)>> {
    let conn = sqlite::open_readonly(unit)?;
    // `columns`/`has_table` tragan los errores: esta consulta distingue una base ilegible
    // (bloqueada, corrupta, no SQLite) de una base con otro esquema.
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0))?;
    let cols = sqlite::columns(&conn, "session_v2");
    if !["id", "directory"].iter().all(|c| cols.iter().any(|x| x == c)) {
        // opencode 1.x u otra versión: no hay nada que listar.
        warn_once(&WARNED_SCHEMA, "opencode: la base no tiene session_v2 con id y directory (¿otra versión?)");
        return Ok(Vec::new());
    }
    // Columnas que pueden faltar según la versión: se piden como NULL.
    let col = |name: &str| if cols.iter().any(|c| c == name) { format!("`{name}`") } else { "NULL".to_string() };
    // Hijas (subagentes) fuera; un fork tiene `parent_id` y `fork_session_id` y se queda.
    let sql = format!(
        "SELECT id, directory, {title}, {slug}, {model}, {created}, {updated}, {idle}, {busy}, {archived} FROM session_v2 \
         WHERE NOT ({parent} IS NOT NULL AND {fork} IS NULL)",
        title = col("title"),
        slug = col("slug"),
        model = col("model"),
        created = col("time_created"),
        updated = col("time_updated"),
        idle = col("time_idle"),
        busy = col("time_busy"),
        archived = col("time_archived"),
        parent = col("parent_id"),
        fork = col("fork_session_id"),
    );
    let mut rows = Vec::new();
    {
        let mut stmt = conn.prepare(&sql)?;
        let mut q = stmt.query([])?;
        while let Some(r) = q.next()? {
            let (Some(id), Some(directory)) = (text(r, 0), text(r, 1)) else { continue };
            rows.push(SessionRow {
                id,
                directory,
                title: text(r, 2),
                slug: text(r, 3),
                model: text(r, 4),
                created: int(r, 5),
                updated: int(r, 6),
                idle: int(r, 7),
                busy: int(r, 8),
                archived: archived(r, 9),
            });
        }
    }
    let msgs = {
        let m = sqlite::columns(&conn, "session_message");
        ["session_id", "type", "seq", "data"].iter().all(|c| m.iter().any(|x| x == c))
    };
    let mut out = Vec::new();
    for row in rows {
        // Archivadas y sin carpeta no se listan (y no se guardan: no se releen).
        if row.archived.is_some() || row.directory.trim().is_empty() {
            continue;
        }
        let key = PathBuf::from(&row.id);
        let old = prev.get(&key);
        // Un fallo en una sesión (página dañada, base ocupada) no tira el resto: se conserva su dato anterior.
        let sig = if msgs { user_sig(&conn, &row.id) } else { Ok((0, 0)) };
        let fp = match &sig {
            Ok(s) => fingerprint(&row, *s),
            Err(_) => 0,
        };
        let built = match sig {
            Ok(_) if old.is_some_and(|p| p.fingerprint == fp && p.file == unit && p.harness == Harness::Opencode) => {
                out.push((key, old.cloned().unwrap_or_default()));
                continue;
            }
            Ok(s) => build(&conn, unit, &row, fp, s.0),
            Err(e) => Err(e),
        };
        match built {
            Ok(m) => out.push((key, m)),
            Err(e) => {
                warn_once(&WARNED_OPEN, &format!("opencode: no se pudo leer una sesión: {e}"));
                if let Some(p) = old {
                    out.push((key, p.clone()));
                }
            }
        }
    }
    Ok(out)
}

/// `(nº de prompts humanos, último seq)`: barato (índice `session_id, type, seq`) y
/// detecta mensajes nuevos aunque la fila de la sesión no cambie.
fn user_sig(conn: &Connection, id: &str) -> rusqlite::Result<(i64, i64)> {
    conn.query_row("SELECT count(*), coalesce(max(seq), 0) FROM session_message WHERE session_id = ?1 AND type = 'user'", [id], |r| {
        Ok((r.get(0)?, r.get(1).unwrap_or(0)))
    })
}

/// FNV-1a de lo que, si cambia, obliga a releer la sesión.
fn fingerprint(r: &SessionRow, sig: (i64, i64)) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for b in bytes.iter().chain(&[0xff]) {
            h = (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for t in [r.created, r.updated, r.idle, r.busy, r.archived] {
        match t {
            Some(v) => eat(&v.to_le_bytes()),
            None => eat(b"-"),
        }
    }
    eat(&sig.0.to_le_bytes());
    eat(&sig.1.to_le_bytes());
    for s in [Some(&r.directory), r.title.as_ref(), r.slug.as_ref(), r.model.as_ref()] {
        eat(s.map_or(b"-".as_slice(), |s| s.as_bytes()));
    }
    h
}

fn build(conn: &Connection, unit: &Path, r: &SessionRow, fp: u64, n: i64) -> rusqlite::Result<SessionMeta> {
    let (first, last) = if n > 0 { (edge_prompt(conn, &r.id, "ASC")?, edge_prompt(conn, &r.id, "DESC")?) } else { (None, None) };
    // Última actividad: `time_idle` (fin del último turno), o `time_busy` si hay un
    // turno en curso. `time_updated` también sube al archivar o mover, por eso solo es respaldo.
    let activity = match (r.idle, r.busy) {
        (Some(i), Some(b)) => Some(i.max(b)),
        (i, b) => i.or(b),
    };
    let busy = match (r.idle, r.busy) {
        (Some(i), Some(b)) => b > i,
        (None, Some(_)) => true,
        _ => false,
    };
    let title = r.title.as_deref().map(collapse).filter(|t| !t.is_empty());
    let slug = r.slug.as_deref().map(collapse).filter(|t| !t.is_empty());
    Ok(SessionMeta {
        harness: Harness::Opencode,
        id: r.id.clone(),
        file: unit.to_path_buf(),
        mtime_ms: activity.or(r.updated).or(r.created).unwrap_or(0),
        fingerprint: fp,
        cwd: Some(r.directory.clone()),
        // El título propio de opencode manda; sin él, el primer prompt y, a falta de este, el slug.
        ai_title: title.or_else(|| first.clone()).or(slug),
        first_prompt: first,
        last_prompt: last,
        started_ms: r.created,
        model: r.model.as_deref().and_then(model_label),
        prompts: u32::try_from(n).unwrap_or(u32::MAX),
        has_messages: n > 0,
        busy,
        ..SessionMeta::default()
    })
}

/// Primer (`ASC`) o último (`DESC`) prompt legible. El texto sale de `$.text` (o
/// `$.metadata.displayText`) y, si una fila no es JSON válido o no tiene texto,
/// se baja a la siguiente.
fn edge_prompt(conn: &Connection, id: &str, order: &str) -> rusqlite::Result<Option<String>> {
    // `json_valid` protege: `json_extract` aborta toda la consulta con un JSON truncado.
    let sql = format!(
        "SELECT CASE WHEN json_valid(data) THEN coalesce(\
            nullif(CASE WHEN json_type(data, '$.text') = 'text' THEN json_extract(data, '$.text') END, ''), \
            CASE WHEN json_type(data, '$.metadata.displayText') = 'text' THEN json_extract(data, '$.metadata.displayText') END) END \
         FROM session_message WHERE session_id = ?1 AND type = 'user' ORDER BY seq {order} LIMIT {MAX_PROMPT_ROWS}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([id])?;
    while let Some(r) = rows.next()? {
        if let Some(t) = text(r, 0).map(|t| collapse(&t)).filter(|t| !t.is_empty()) {
            return Ok(Some(t));
        }
    }
    Ok(None)
}

/// `proveedor/modelo#variante` a partir del JSON `{id, providerID, variant}`.
fn model_label(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        // No es JSON: se muestra tal cual si parece un nombre de modelo.
        return Some(raw.to_string()).filter(|s| !s.is_empty() && !s.starts_with(['{', '[']));
    };
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::trim).filter(|s| !s.is_empty());
    let mut s = String::new();
    if let Some(p) = field("providerID") {
        s.push_str(p);
        s.push('/');
    }
    s.push_str(field("id").or_else(|| field("modelID"))?);
    if let Some(var) = field("variant") {
        s.push('#');
        s.push_str(var);
    }
    Some(s)
}

/// La TUI raíz solo admite `--session` y `--auto`. Sin fork: se reanuda. No se
/// preasigna id (`-s` con un id nuevo no está verificado).
pub fn args(mode: &Mode, opts: &LaunchOptions) -> (Vec<String>, Option<String>) {
    let mut a = Vec::new();
    if opts.permission_mode.as_deref() == Some("auto") {
        a.push("--auto".to_string());
    }
    match mode {
        Mode::New => (a, None),
        Mode::Resume(id) | Mode::Fork(id) => {
            a.push("--session".to_string());
            a.push(id.clone());
            (a, Some(id.clone()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static N: AtomicUsize = AtomicUsize::new(0);

    /// Carpeta temporal única con la base `opencode.db`; se borra al soltarla.
    struct Tmp {
        dir: PathBuf,
    }

    impl Tmp {
        fn new() -> Tmp {
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join("claunch-tests").join(format!("opencode-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Tmp { dir }
        }
        fn db(&self) -> PathBuf {
            self.dir.join("opencode.db")
        }
        fn conn(&self) -> Connection {
            Connection::open(self.db()).unwrap()
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const DDL: &str = "
CREATE TABLE `session_v2` (
  `id` text PRIMARY KEY, `project_id` text NOT NULL, `workspace_id` text, `parent_id` text, `fork_session_id` text, `fork_boundary` text,
  `slug` text NOT NULL, `directory` text NOT NULL, `path` text, `title` text, `version` text NOT NULL, `agent` text, `model` text,
  `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `time_idle` integer, `time_viewed` integer, `idle_outcome` text,
  `time_archived` integer, `resume_attempts` integer DEFAULT 0 NOT NULL, `time_busy` integer);
CREATE TABLE `session_message` (`id` text PRIMARY KEY, `session_id` text NOT NULL, `type` text NOT NULL, `seq` integer NOT NULL,
  `time_created` integer NOT NULL, `time_updated` integer NOT NULL, `data` text NOT NULL);
CREATE UNIQUE INDEX `session_message_session_seq_idx` ON `session_message` (`session_id`,`seq`);
CREATE INDEX `session_message_session_type_seq_idx` ON `session_message` (`session_id`,`type`,`seq`);";

    // Contenido inventado con la estructura real (sección 10 de la especificación).
    const ROWS: &str = r#"
INSERT INTO session_v2(id,project_id,parent_id,slug,directory,path,title,version,agent,model,time_created,time_updated,time_idle,time_viewed,idle_outcome,time_busy,resume_attempts)
 VALUES ('ses_aaa','p',NULL,'happy-fox','C:/Users/demo/proj-a','','Titulo ejemplo','2.2.5','build',
 '{"id":"modelo-demo","providerID":"proveedor-demo","variant":"alta"}',1790000000000,1790000900000,1790000800000,1790000800000,'succeeded',1790000100000,0);
INSERT INTO session_v2(id,project_id,parent_id,slug,directory,path,title,version,agent,model,time_created,time_updated,time_idle,idle_outcome,time_busy,resume_attempts)
 VALUES ('ses_bbb','p','ses_aaa','calm-owl','C:/Users/demo/proj-a','','Subtarea','2.2.5','explore',NULL,1790000200000,1790000300000,1790000300000,'succeeded',1790000200000,0);
INSERT INTO session_v2(id,project_id,slug,directory,path,title,version,agent,time_created,time_updated,time_idle,time_archived,idle_outcome,resume_attempts)
 VALUES ('ses_ccc','p','old-ant','C:/Users/demo/proj-a','','Vieja','2.0.32','build',1789000000000,1789000900000,1789000800000,1789000900000,'interrupted',0);
INSERT INTO session_v2(id,project_id,parent_id,fork_session_id,fork_boundary,slug,directory,path,title,version,agent,time_created,time_updated,time_idle,resume_attempts)
 VALUES ('ses_ddd','p','ses_aaa','ses_aaa','{"type":"before","messageID":"msg_x"}','fork-fox','C:/Users/demo/proj-a','',NULL,'2.2.5','build',1790001000000,1790001100000,1790001100000,0);
INSERT INTO session_v2(id,project_id,slug,directory,path,title,version,agent,time_created,time_updated,resume_attempts)
 VALUES ('ses_eee','p','empty-emu','C:/Users/demo/proj-a','','','2.2.5','build',1790002000000,1790002000000,0);
INSERT INTO session_v2(id,project_id,slug,directory,path,title,version,time_created,time_updated,resume_attempts)
 VALUES ('ses_fff','p','no-dir','','','Sin carpeta','2.2.5',1790003000000,1790003000000,0);
INSERT INTO session_message(id,session_id,type,seq,time_created,time_updated,data) VALUES
 ('m1','ses_aaa','user',4,1790000050000,1790000050000,'{"metadata":{"displayText":"hola mundo","agent":"build"},"time":{"created":1},"text":"hola   mundo","files":[],"agents":[]}'),
 ('m2','ses_aaa','assistant',5,1790000060000,1790000070000,'{"agent":"build","content":[{"type":"reasoning","text":"no soy un prompt"}]}'),
 ('m3','ses_aaa','idle',10,1790000800000,1790000800000,'{"outcome":"succeeded"}'),
 ('m4','ses_aaa','synthetic',15,1790000810000,1790000810000,'{"text":"sintetico"}'),
 ('m5','ses_aaa','user',21,1790000850000,1790000850000,'{"metadata":{"displayText":"segundo prompt"},"text":"segundo prompt"}'),
 ('b1','ses_ddd','user',2,1790001050000,1790001050000,'{"text":"fork prompt","time":{"created":1}}'),
 ('b2','ses_ddd','user',3,1790001060000,1790001060000,'{trunc'),
 ('x1','ses_bbb','user',2,1790000250000,1790000250000,'{"text":"de la hija"}'),
 ('x2','ses_ccc','user',2,1789000250000,1789000250000,'{"text":"de la archivada"}');"#;

    fn fixture() -> Tmp {
        let t = Tmp::new();
        let c = t.conn();
        c.execute_batch(DDL).unwrap();
        c.execute_batch(ROWS).unwrap();
        t
    }

    fn run(t: &Tmp, prev: &Prev) -> Prev {
        index(&t.dir, &t.db(), prev).into_iter().collect()
    }

    fn key(id: &str) -> PathBuf {
        PathBuf::from(id)
    }

    #[test]
    fn discovers_and_applies_exclusions() {
        let t = fixture();
        let m = run(&t, &Prev::new());
        let mut ids: Vec<_> = m.keys().map(|k| k.to_string_lossy().into_owned()).collect();
        ids.sort();
        // Hija, archivada y sin carpeta fuera; el fork (parent_id + fork_session_id) y la vacía dentro.
        assert_eq!(ids, ["ses_aaa", "ses_ddd", "ses_eee"]);
        // La clave es el id, como la espera Harness::index_key.
        for (k, v) in &m {
            assert_eq!(*k, Harness::Opencode.index_key(v));
            assert_eq!(v.harness, Harness::Opencode);
            assert_eq!(v.file, t.db());
            assert_eq!(v.size, 0);
            assert!(v.entrypoint.is_none() && v.git_branch.is_none() && !v.hidden);
        }
        // Sin pasar por el entorno: `OPENCODE_DB` de quien ejecuta los tests no cuenta.
        assert_eq!(units_in(&t.dir, None), [t.db()]);
        assert_eq!(units_in(&t.dir, Some("otra.db")), Vec::<PathBuf>::new());
    }

    #[test]
    fn maps_fields_and_counts_only_human_prompts() {
        let t = fixture();
        let m = run(&t, &Prev::new());
        let a = &m[&key("ses_aaa")];
        assert_eq!(a.cwd.as_deref(), Some("C:/Users/demo/proj-a"));
        assert_eq!(a.prompts, 2, "assistant, idle y synthetic no cuentan");
        assert_eq!(a.first_prompt.as_deref(), Some("hola mundo"));
        assert_eq!(a.last_prompt.as_deref(), Some("segundo prompt"));
        assert_eq!(a.ai_title.as_deref(), Some("Titulo ejemplo"));
        assert_eq!(a.title(), "Titulo ejemplo");
        assert!(a.has_messages);
        assert_eq!(a.started_ms, Some(1790000000000));
        // Última actividad = time_idle, no time_updated (que sube al archivar o mover).
        assert_eq!(a.mtime_ms, 1790000800000);
        assert_eq!(a.model.as_deref(), Some("proveedor-demo/modelo-demo#alta"));
        assert!(!a.busy);
    }

    #[test]
    fn title_priority_and_unreadable_rows() {
        let t = fixture();
        let m = run(&t, &Prev::new());
        // Fork: título nulo -> primer prompt; la fila truncada se salta en ambos extremos.
        let d = &m[&key("ses_ddd")];
        assert_eq!(d.prompts, 2);
        assert_eq!(d.ai_title.as_deref(), Some("fork prompt"));
        assert_eq!(d.first_prompt.as_deref(), Some("fork prompt"));
        assert_eq!(d.last_prompt.as_deref(), Some("fork prompt"));
        assert!(d.has_messages);
        // Sin prompts y con título vacío: slug; no se lista (has_messages = false).
        let e = &m[&key("ses_eee")];
        assert_eq!(e.ai_title.as_deref(), Some("empty-emu"));
        assert!(!e.has_messages && e.prompts == 0 && e.first_prompt.is_none());
        // Sin time_idle ni time_busy: respaldo time_updated.
        assert_eq!(e.mtime_ms, 1790002000000);
    }

    #[test]
    fn text_fallback_to_display_text() {
        let t = fixture();
        t.conn()
            .execute_batch(
                r#"INSERT INTO session_message VALUES ('e1','ses_eee','user',1,1,1,'{"text":"","metadata":{"displayText":"desde displayText"}}');"#,
            )
            .unwrap();
        let m = run(&t, &Prev::new());
        assert_eq!(m[&key("ses_eee")].first_prompt.as_deref(), Some("desde displayText"));
    }

    #[test]
    fn busy_follows_last_saved_state() {
        let t = fixture();
        t.conn()
            .execute_batch("UPDATE session_v2 SET time_busy = 1790000900000 WHERE id = 'ses_aaa'; UPDATE session_v2 SET time_busy = 5 WHERE id = 'ses_ddd';")
            .unwrap();
        let m = run(&t, &Prev::new());
        let a = &m[&key("ses_aaa")];
        assert!(a.busy);
        assert_eq!(a.mtime_ms, 1790000900000);
        assert!(!m[&key("ses_ddd")].busy);
    }

    #[test]
    fn incremental_equals_full_after_changes() {
        let t = fixture();
        let first = run(&t, &Prev::new());
        // Misma base, mismo resultado y sin cambios.
        assert_eq!(run(&t, &first), first);
        // Llega un prompt nuevo y la sesión cambia de tiempos; llega una sesión nueva.
        t.conn()
            .execute_batch(
                r#"INSERT INTO session_message VALUES ('m9','ses_aaa','user',30,1790001500000,1790001500000,'{"text":"tercero"}');
                 UPDATE session_v2 SET time_updated = 1790001600000, time_idle = 1790001550000, title = 'Otro título' WHERE id = 'ses_aaa';
                 INSERT INTO session_v2(id,project_id,slug,directory,title,version,time_created,time_updated,time_idle,resume_attempts)
                  VALUES ('ses_ggg','p','nueva','/home/x/y','Nueva','2.2.5',1790004000000,1790004000000,1790004000000,0);
                 INSERT INTO session_message VALUES ('g1','ses_ggg','user',1,1,1,'{"text":"primero"}');"#,
            )
            .unwrap();
        let incr = run(&t, &first);
        let full = run(&t, &Prev::new());
        assert_eq!(incr, full);
        let a = &full[&key("ses_aaa")];
        assert_eq!((a.prompts, a.last_prompt.as_deref(), a.ai_title.as_deref()), (3, Some("tercero"), Some("Otro título")));
        assert_eq!(full.len(), 4);
        // Desaparece una sesión (borrada): sale del resultado.
        t.conn().execute_batch("DELETE FROM session_v2 WHERE id = 'ses_ggg';").unwrap();
        assert!(!run(&t, &full).contains_key(&key("ses_ggg")));
    }

    #[test]
    fn unchanged_fingerprint_reuses_previous_meta_without_rereading_text() {
        let t = fixture();
        let first = run(&t, &Prev::new());
        // Mismo nº de prompts y mismo seq: no se vuelve a leer el texto (se nota porque el cambio no aparece).
        t.conn().execute_batch(r#"UPDATE session_message SET data = '{"text":"editado"}' WHERE id = 'm1';"#).unwrap();
        assert_eq!(run(&t, &first)[&key("ses_aaa")].first_prompt.as_deref(), Some("hola mundo"));
        // Un prompt nuevo sin tocar la fila de la sesión sí se ve (no está verificado que time_updated suba).
        t.conn().execute_batch(r#"INSERT INTO session_message VALUES ('m9','ses_aaa','user',30,1,1,'{"text":"nuevo"}');"#).unwrap();
        let m = run(&t, &first);
        let a = &m[&key("ses_aaa")];
        assert_eq!((a.prompts, a.last_prompt.as_deref(), a.first_prompt.as_deref()), (3, Some("nuevo"), Some("editado")));
        // Un cambio en la huella (time_idle) también fuerza la relectura.
        t.conn().execute_batch("UPDATE session_v2 SET time_idle = time_idle + 1 WHERE id = 'ses_ddd';").unwrap();
        assert_eq!(run(&t, &first)[&key("ses_ddd")].mtime_ms, 1790001100001);
    }

    #[test]
    fn passes_are_deterministic_and_chunked_equals_full() {
        let t = fixture();
        let a = run(&t, &Prev::new());
        let b = run(&t, &Prev::new());
        assert_eq!(a, b);
        assert_eq!(run(&t, &a), a);
        // Los mismos resultados en el mismo orden de claves tras otra pasada incremental.
        let c = run(&t, &run(&t, &a));
        assert_eq!(c, a);
    }

    #[test]
    fn odd_directories_and_ids_pass_through_untouched() {
        let t = fixture();
        let c = t.conn();
        for (id, dir) in [("u1", r"\srv\share\proy"), ("u2", "/home/ñandú/con espacios/"), ("u3", "C:/Users/x/proy/"), ("u4", "  ")] {
            c.execute("INSERT INTO session_v2(id,project_id,slug,directory,title,version,time_created,time_updated,resume_attempts) VALUES (?1,'p','s',?2,'T','2',1,1,0)", [id, dir]).unwrap();
        }
        let m = run(&t, &Prev::new());
        assert_eq!(m[&key("u1")].cwd.as_deref(), Some(r"\srv\share\proy"));
        assert_eq!(m[&key("u2")].cwd.as_deref(), Some("/home/ñandú/con espacios/"));
        assert_eq!(m[&key("u3")].cwd.as_deref(), Some("C:/Users/x/proy/"));
        assert!(!m.contains_key(&key("u4")), "directory en blanco no se lista");
    }

    #[test]
    fn corrupt_message_data_types_do_not_abort() {
        let t = fixture();
        let c = t.conn();
        c.execute_batch(
            r#"INSERT INTO session_message VALUES ('z1','ses_eee','user',1,1,1,'null');
               INSERT INTO session_message VALUES ('z2','ses_eee','user',2,1,1,'[1,2]');
               INSERT INTO session_message VALUES ('z3','ses_eee','user',3,1,1,'{"text":42}');
               INSERT INTO session_message VALUES ('z4','ses_eee','user',4,1,1,'{"text":{"a":1}}');
               INSERT INTO session_message VALUES ('z5','ses_eee','user',5,1,1,'');
               INSERT INTO session_message VALUES ('z6','ses_eee','user',6,1,1,x'ff00fe');
               INSERT INTO session_message VALUES ('z7','ses_eee','user',7,1,1,'{"text":"   
  "}');"#,
        )
        .unwrap();
        let m = run(&t, &Prev::new());
        let e = &m[&key("ses_eee")];
        assert_eq!(e.prompts, 7, "cuentan aunque no se lean");
        assert!(e.first_prompt.is_none() && e.last_prompt.is_none());
        assert!(e.has_messages);
        // Con un texto legible entre la basura, se encuentra.
        c.execute_batch(r#"INSERT INTO session_message VALUES ('z8','ses_eee','user',0,1,1,'{"text":"vivo"}');"#).unwrap();
        assert_eq!(run(&t, &Prev::new())[&key("ses_eee")].first_prompt.as_deref(), Some("vivo"));
    }

    #[test]
    fn multi_megabyte_prompt_is_cheap_and_truncated() {
        let t = fixture();
        let big = "x".repeat(6_000_000);
        t.conn().execute("INSERT INTO session_message VALUES ('big','ses_eee','user',1,1,1,?1)", [format!("{{\"text\":\"{big}\"}}")]).unwrap();
        let t0 = std::time::Instant::now();
        let m = run(&t, &Prev::new());
        assert!(t0.elapsed().as_secs() < 5);
        assert!(m[&key("ses_eee")].first_prompt.as_deref().unwrap().chars().count() < 1000);
    }

    #[test]
    fn missing_message_table_and_unparsable_archive_marker() {
        let t = Tmp::new();
        t.conn()
            .execute_batch(
                "CREATE TABLE session_v2 (id text PRIMARY KEY, directory text, time_archived text);
                 INSERT INTO session_v2 VALUES ('a', '/x', 'rara');
                 INSERT INTO session_v2 VALUES ('b', '/y', NULL);",
            )
            .unwrap();
        let m = run(&t, &Prev::new());
        assert!(!m.contains_key(&key("a")), "un marcador de archivado ilegible sigue siendo archivada");
        assert!(m.contains_key(&key("b")));
    }

    #[test]
    fn empty_database_and_locked_database_keep_previous() {
        let t = Tmp::new();
        t.conn().execute_batch("CREATE TABLE x (a);").unwrap();
        assert!(run(&t, &Prev::new()).is_empty());
        let t = fixture();
        let first = run(&t, &Prev::new());
        // Escritor con bloqueo exclusivo: la lectura no debe dejar al usuario sin sesiones.
        let w = t.conn();
        w.execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE;").unwrap();
        let during = run(&t, &first);
        w.execute_batch("ROLLBACK;").unwrap();
        assert_eq!(during, first);
    }

    #[test]
    fn archived_and_restored_sessions_follow_the_row() {
        let t = fixture();
        let first = run(&t, &Prev::new());
        t.conn().execute_batch("UPDATE session_v2 SET time_archived = 1790009000000 WHERE id = 'ses_aaa';").unwrap();
        assert!(!run(&t, &first).contains_key(&key("ses_aaa")));
        t.conn().execute_batch("UPDATE session_v2 SET time_archived = NULL WHERE id = 'ses_ccc';").unwrap();
        // ses_ccc tiene parent_id nulo: al desarchivarla aparece.
        assert!(run(&t, &first).contains_key(&key("ses_ccc")));
    }

    #[test]
    fn unreadable_database_returns_previous() {
        let t = fixture();
        let first = run(&t, &Prev::new());
        assert_eq!(first.len(), 3);
        // Archivo que no es SQLite.
        std::fs::write(t.db(), "esto no es una base de datos, ni de lejos; solo texto que ocupa lo suyo".repeat(10)).unwrap();
        assert_eq!(run(&t, &first), first);
        // Base que ya no existe.
        std::fs::remove_file(t.db()).unwrap();
        assert_eq!(run(&t, &first), first);
        assert!(units_in(&t.dir, None).is_empty());
        assert!(run(&t, &Prev::new()).is_empty());
    }

    #[test]
    fn other_schema_yields_no_sessions() {
        let t = Tmp::new();
        t.conn().execute_batch("CREATE TABLE session (id text PRIMARY KEY, directory text); INSERT INTO session VALUES ('s','/x');").unwrap();
        assert!(run(&t, &Prev::new()).is_empty());
    }

    #[test]
    fn tolerates_missing_columns_and_tables() {
        let t = Tmp::new();
        t.conn()
            .execute_batch(
                "CREATE TABLE session_v2 (id text PRIMARY KEY, directory text NOT NULL, time_created integer);
                 INSERT INTO session_v2 VALUES ('s1', '/work/a', 1790000000000);",
            )
            .unwrap();
        let m = run(&t, &Prev::new());
        let s = &m[&key("s1")];
        assert_eq!((s.cwd.as_deref(), s.mtime_ms, s.has_messages, s.started_ms), (Some("/work/a"), 1790000000000, false, Some(1790000000000)));
        // Con session_message pero sin sus columnas: sigue sin fallar.
        t.conn().execute_batch("CREATE TABLE session_message (id text);").unwrap();
        assert_eq!(run(&t, &Prev::new()).len(), 1);
    }

    #[test]
    fn huge_prompt_is_truncated_and_odd_value_types_are_tolerated() {
        let t = fixture();
        let big = "palabra ".repeat(200_000);
        let c = t.conn();
        c.execute("INSERT INTO session_message VALUES ('big','ses_eee','user',1,1,1,?1)", [format!("{{\"text\":\"{big}\"}}")]).unwrap();
        // Tiempos como real y texto en una fila: no deben abortar la lectura.
        c.execute_batch(
            "INSERT INTO session_v2(id,project_id,slug,directory,title,version,time_created,time_updated,time_idle,resume_attempts)
              VALUES ('ses_hhh','p','raro','/r','Raro','2.2.5',1790000000000.0,'1790000000001',1790000000002.5,0);",
        )
        .unwrap();
        let m = run(&t, &Prev::new());
        assert!(m[&key("ses_eee")].first_prompt.as_deref().unwrap().chars().count() < 1000);
        let h = &m[&key("ses_hhh")];
        assert_eq!((h.started_ms, h.mtime_ms), (Some(1790000000000), 1790000000002));
    }

    #[test]
    fn model_labels() {
        assert_eq!(model_label(r#"{"id":"m","providerID":"p","variant":"v"}"#).as_deref(), Some("p/m#v"));
        assert_eq!(model_label(r#"{"id":"m"}"#).as_deref(), Some("m"));
        assert_eq!(model_label(r#"{"providerID":"p"}"#), None);
        assert_eq!(model_label("p/m").as_deref(), Some("p/m"));
        assert_eq!(model_label("{roto"), None);
        assert_eq!(model_label(""), None);
    }

    #[test]
    fn database_path_resolution() {
        let home = Path::new("datos");
        assert_eq!(db_path(home, None), home.join("opencode.db"));
        assert_eq!(db_path(home, Some("")), home.join("opencode.db"));
        assert_eq!(db_path(home, Some(":memory:")), home.join("opencode.db"));
        assert_eq!(db_path(home, Some("otra.db")), home.join("otra.db"));
        let abs = std::env::temp_dir().join("abs.db");
        assert_eq!(db_path(home, Some(abs.to_str().unwrap())), abs);
    }

    #[test]
    fn args_for_new_resume_and_fork() {
        let none = LaunchOptions::default();
        assert_eq!(args(&Mode::New, &none), (vec![], None));
        let resume = (vec!["--session".to_string(), "ses_a".to_string()], Some("ses_a".to_string()));
        assert_eq!(args(&Mode::Resume("ses_a".into()), &none), resume);
        // Sin fork en la TUI raíz: se reanuda, aunque se pida bifurcar, con modelo o con nombre.
        let opts = LaunchOptions { fork: true, model: Some("x/y".into()), name: Some("nombre".into()), worktree: Some("wt".into()), ..LaunchOptions::default() };
        assert_eq!(args(&Mode::Fork("ses_a".into()), &opts), resume);
        assert_eq!(args(&Mode::New, &opts), (vec![], None));
        // Permisos: "auto" -> --auto; el resto se ignora.
        let auto = LaunchOptions { permission_mode: Some("auto".into()), ..LaunchOptions::default() };
        assert_eq!(args(&Mode::New, &auto).0, ["--auto"]);
        assert_eq!(args(&Mode::Resume("ses_a".into()), &auto).0, ["--auto", "--session", "ses_a"]);
        for p in ["", "plan", "bypass", "acceptEdits"] {
            let o = LaunchOptions { permission_mode: Some(p.into()), ..LaunchOptions::default() };
            assert!(args(&Mode::New, &o).0.is_empty(), "{p}");
        }
        assert!(INFO.permissions.iter().all(|p| p.value.is_empty() || p.value == "auto"));
    }
}
