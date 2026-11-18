//! OpenAI Codex CLI: sesiones en `~/.codex/sessions/AAAA/MM/DD/rollout-*.jsonl`.
//!
//! Cada hilo es un rollout JSONL que solo crece por el final. La primera línea
//! es `session_meta` (id, cwd, origen) y de ahí en adelante `event_msg`
//! (`task_started`, `item_completed`…), `turn_context` (modelo) y
//! `response_item`. Esta última es con diferencia la más pesada (hay líneas de
//! varios MB), así que cada línea se clasifica mirando solo sus primeros bytes
//! y únicamente se deserializan las pocas que aportan datos.
//!
//! Los nombres de hilo (`/rename`) viven aparte, en `session_index.jsonl`:
//! `finish` los aplica tras indexar. Los rollouts son la única fuente: no se
//! abre ninguna base SQLite de Codex ni se sondean sus locks.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Deserialize;
use serde_json::Value;

use super::{Harness, Info, ModelInput, Perm, Prev};
use crate::config::LaunchOptions;
use crate::data::sessions::{self, SessionMeta, collapse};
use crate::launch::Mode;

pub const INFO: Info = Info {
    id: "codex",
    label: "Codex",
    tag: "codex",
    program: "codex",
    install: "npm install -g @openai/codex",
    color: "#7FC4C0",
    fork: true,
    name: false,
    worktree: false,
    model: ModelInput::Free("gpt-5.5"),
    permissions: &[
        Perm { value: "", label: "por defecto", hint: "" },
        Perm { value: "ask", label: "preguntar", hint: "edita el proyecto y pregunta lo demás" },
        Perm { value: "read-only", label: "solo lectura", hint: "no modifica nada" },
        Perm { value: "never-ask", label: "sin preguntar", hint: "nunca pregunta; sandbox en el proyecto" },
        Perm { value: "bypass", label: "sin sandbox", hint: "sin sandbox ni aprobaciones: peligroso" },
    ],
};

/// Lo más grande que se analiza de una línea; las que lo superan (salidas de
/// herramientas de hasta 6 MB) se saltan sin tocar el resto del archivo.
const MAX_LINE: usize = 8 << 20;
/// Bytes del principio de la línea donde se leen `type` y `payload.type`.
const HEAD: usize = 400;
/// Marca que el IDE antepone al texto real del usuario.
const REQUEST_MARK: &str = "## My request for Codex:";
const IDE_PREFIX: &str = "# Context from my IDE setup:";
/// Envoltorios de contexto que Codex puede guardar como mensaje de usuario sin que lo haya escrito nadie.
const SYSTEM_TAGS: [&str; 7] = [
    "<environment_context",
    "<user_instructions",
    "<turn_aborted",
    "<permissions",
    "<collaboration_mode",
    "<subagent_notification",
    "<skill>",
];
/// Profundidad máxima del árbol `sessions/` (AAAA/MM/DD son 3).
const MAX_DEPTH: usize = 8;

static WARNED_UNITS: AtomicBool = AtomicBool::new(false);
static WARNED_NAMES: AtomicBool = AtomicBool::new(false);

/// Carpeta de datos (`CODEX_HOME` o `~/.codex`).
pub fn home() -> PathBuf {
    match std::env::var_os("CODEX_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => crate::paths::home().join(".codex"),
    }
}

#[cfg(windows)]
const EXE: &str = "codex.exe";
#[cfg(not(windows))]
const EXE: &str = "codex";

/// Ejecutable de Codex fuera del PATH (el que instalan sus aplicaciones), si lo hay.
pub fn locate() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty()).map(PathBuf::from);
    locate_in(local.as_deref(), &crate::paths::home(), &home())
}

/// Busca el ejecutable en las ubicaciones habituales y se queda con el más
/// reciente: un Codex nuevo reanuda sesiones de uno antiguo, pero no al revés.
fn locate_in(local_app_data: Option<&Path>, user_home: &Path, codex_home: &Path) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    // App de escritorio (Windows): %LOCALAPPDATA%\OpenAI\Codex\bin\<hash>\codex.exe
    if let Some(local) = local_app_data {
        dirs.extend(subdirs(&local.join("OpenAI").join("Codex").join("bin"), |_| true));
    }
    // Extensión de VS Code: .vscode/extensions/openai.chatgpt-<ver>/bin/<plataforma>/codex
    let plat = if cfg!(windows) { "windows" } else { "linux" };
    let exts = subdirs(&user_home.join(".vscode").join("extensions"), |n| n.starts_with("openai.chatgpt-"));
    dirs.extend(exts.into_iter().flat_map(|e| subdirs(&e.join("bin"), |n| n.starts_with(plat))));
    // Instalación gestionada y local del usuario.
    dirs.push(codex_home.join("packages").join("app-server-daemon").join("current").join("bin"));
    dirs.push(user_home.join(".local").join("bin"));
    newest_exe(dirs)
}

/// Subcarpetas de `dir` cuyo nombre cumple `keep`.
fn subdirs(dir: &Path, keep: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && e.file_name().to_str().is_some_and(&keep)).map(|e| e.path()).collect()
}

/// El `codex` más recientemente modificado de entre las carpetas dadas.
fn newest_exe(dirs: Vec<PathBuf>) -> Option<PathBuf> {
    dirs.into_iter()
        .map(|d| d.join(EXE))
        .filter_map(|p| std::fs::metadata(&p).ok().filter(|m| m.is_file()).map(|m| (sessions::mtime_ms(&m), p)))
        .max()
        .map(|(_, p)| p)
}

// ---------------------------------------------------------------- unidades

/// Id del hilo a partir del nombre `rollout-AAAA-MM-DDThh-mm-ss-<uuid>[_<uuid>].jsonl`.
/// Los hilos revertidos añaden un segundo UUID tras `_`; el del hilo es el primero.
fn thread_id_of(name: &str) -> Option<&str> {
    let rest = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    // La fecha ocupa 19 caracteres y va seguida de `-`.
    let id = rest.get(20..56)?;
    let tail = rest.get(56..)?;
    let is_uuid = |s: &str| s.len() == 36 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    let ok_tail = tail.is_empty() || tail.strip_prefix('_').is_some_and(is_uuid);
    (rest.as_bytes().get(19) == Some(&b'-') && is_uuid(id) && ok_tail).then_some(id)
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        // `file_type` de la entrada no sigue enlaces simbólicos: se ignoran.
        let Ok(t) = e.file_type() else { continue };
        if t.is_dir() {
            if depth < MAX_DEPTH {
                walk(&e.path(), depth + 1, out);
            }
        } else if t.is_file()
            && let Some(id) = e.file_name().to_str().and_then(thread_id_of)
        {
            out.push((id.to_string(), e.path()));
        }
    }
}

/// Los rollouts bajo `sessions/` (los de `archived_sessions/` no se listan).
/// Un hilo con varios rollouts (revertido) cuenta una vez: el más reciente.
pub fn units(home: &Path) -> Vec<PathBuf> {
    let root = home.join("sessions");
    if !root.is_dir() {
        // Sin Codex (o sin sesiones) no hay nada que avisar.
        return Vec::new();
    }
    let mut found = Vec::new();
    walk(&root, 0, &mut found);
    let mut by_id: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for (id, p) in found {
        by_id.entry(id).or_default().push(p);
    }
    let mut out: Vec<PathBuf> = by_id
        .into_values()
        .filter_map(|mut v| {
            if v.len() > 1 {
                v.sort_by_cached_key(|p| (std::fs::metadata(p).map(|m| sessions::mtime_ms(&m)).unwrap_or(0), p.clone()));
            }
            v.pop()
        })
        .collect();
    out.sort();
    out
}

// ---------------------------------------------------------------- índice

/// Indexa (o actualiza) un rollout; si falla, conserva lo que había.
pub fn index(_home: &Path, unit: &Path, prev: &Prev) -> Vec<(PathBuf, SessionMeta)> {
    let old = prev.get(unit);
    let meta = match sessions::index_jsonl(unit, old, Harness::Codex, on_line) {
        Ok(mut m) => {
            fix_id(&mut m, unit);
            Some(m)
        }
        Err(e) => {
            if !WARNED_UNITS.swap(true, Ordering::Relaxed) {
                crate::warn_log!("no se pudo indexar {}: {e}", unit.display());
            }
            old.cloned()
        }
    };
    meta.map(|m| (unit.to_path_buf(), m)).into_iter().collect()
}

/// Si el rollout no trajo su id (sin `session_meta`), usa el UUID del nombre.
fn fix_id(meta: &mut SessionMeta, unit: &Path) {
    let name = unit.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let stem = unit.file_stem().and_then(|n| n.to_str()).unwrap_or("");
    if meta.id == stem
        && let Some(id) = thread_id_of(name)
    {
        meta.id = id.to_string();
    }
}

/// Línea con solo lo que interesa de los registros ligeros (`turn_context` y
/// `event_msg`). Los campos escalares son `Value` para que un tipo inesperado
/// en uno de ellos no tire la línea entera; lo demás (instrucciones, herramientas…) se salta.
#[derive(Deserialize)]
struct Line {
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    cwd: Option<Value>,
    model: Option<Value>,
    item: Option<Item>,
    thread_settings: Option<Settings>,
    /// Variante antigua `user_message`.
    message: Option<Value>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(rename = "type")]
    kind: Option<Value>,
    content: Option<Content>,
}

/// `content` de un mensaje: lista de partes o, en variantes raras, un texto suelto.
#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Parts(Vec<Part>),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
struct Part {
    #[serde(rename = "type")]
    kind: Option<Value>,
    text: Option<Value>,
}

#[derive(Deserialize)]
struct Settings {
    model: Option<Value>,
}

/// Valor de cadena que sigue a `key` (p. ej. `"type":"`) dentro de `head`.
fn str_after<'a>(head: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let at = memchr::memmem::find(head, key)? + key.len();
    let rest = &head[at..];
    Some(&rest[..memchr::memchr(b'"', rest)?])
}

/// ¿Ya se leyó el encabezado? Al crear la sesión `id` es el nombre del archivo
/// (`rollout-…`); el encabezado lo cambia por el UUID del hilo.
fn header_done(meta: &SessionMeta) -> bool {
    !meta.id.starts_with("rollout-")
}

fn on_line(line: &[u8], meta: &mut SessionMeta) {
    // Un hilo oculto se queda así: no hace falta leer más de él.
    if meta.hidden || line.len() > MAX_LINE {
        return;
    }
    let head = &line[..line.len().min(HEAD)];
    let Some(top) = str_after(head, b"\"type\":\"") else { return };
    match top {
        b"session_meta" => {
            // Un solo encabezado por rollout; los copiados de un padre no cuentan.
            if !header_done(meta)
                && let Ok(v) = serde_json::from_slice::<Value>(line)
            {
                apply_header(&v, meta);
            }
        }
        b"turn_context" => {
            if let Some(p) = parse(line).and_then(|l| l.payload) {
                set_model(meta, p.model.as_ref());
                if meta.cwd.is_none() {
                    meta.cwd = p.cwd.as_ref().and_then(Value::as_str).map(strip_verbatim).filter(|c| !c.is_empty());
                }
            }
        }
        b"event_msg" => {
            let Some(kind) = str_after(head, b"\"payload\":{\"type\":\"") else { return };
            match kind {
                b"task_started" => meta.busy = true,
                b"task_complete" | b"turn_aborted" => meta.busy = false,
                b"item_completed" if memchr::memmem::find(line, b"\"UserMessage\"").is_some() => {
                    if let Some(p) = parse(line).and_then(|l| l.payload)
                        && let Some(item) = p.item.filter(|i| i.kind.as_ref().and_then(Value::as_str) == Some("UserMessage"))
                    {
                        add_prompt(meta, item.content);
                    }
                }
                b"user_message" => {
                    if let Some(p) = parse(line).and_then(|l| l.payload)
                        && let Some(Value::String(msg)) = p.message
                    {
                        add_prompt(meta, Some(Content::Text(msg)));
                    }
                }
                b"thread_settings_applied" => {
                    if let Some(s) = parse(line).and_then(|l| l.payload).and_then(|p| p.thread_settings) {
                        set_model(meta, s.model.as_ref());
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn parse(line: &[u8]) -> Option<Line> {
    serde_json::from_slice(line).ok()
}

fn set_model(meta: &mut SessionMeta, model: Option<&Value>) {
    if let Some(m) = model.and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty()) {
        meta.model = Some(m.to_string());
    }
}

/// Quita el prefijo verbatim de Windows (`\\?\C:\x` -> `C:\x`, `\\?\UNC\s\x` -> `\\s\x`).
fn strip_verbatim(cwd: &str) -> String {
    let cwd = cwd.trim();
    if let Some(unc) = cwd.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{unc}");
    }
    cwd.strip_prefix(r"\\?\").unwrap_or(cwd).to_string()
}

type Obj = serde_json::Map<String, Value>;

/// Cadena no vacía de `key`.
fn text_of<'a>(o: &'a Obj, key: &str) -> Option<&'a str> {
    o.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// ¿Es el `source` de una sesión interactiva? Ausente cuenta como `vscode`.
fn interactive_source(source: Option<&Value>) -> bool {
    match source {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => matches!(s.as_str(), "cli" | "vscode"),
        Some(Value::Object(o)) => o.get("custom").and_then(Value::as_str).is_some_and(|c| matches!(c, "atlas" | "chatgpt")),
        Some(_) => false,
    }
}

/// `true` si el hilo lo creó una persona en una sesión interactiva (no un
/// subagente, guardian, `exec`, `mcp`…). Un fork del usuario sí cuenta.
fn is_user_thread(p: &Obj) -> bool {
    let source = p.get("source");
    let subagent_source = source.and_then(Value::as_object).is_some_and(|o| o.contains_key("subagent"));
    let child = ["parent_thread_id", "agent_nickname", "agent_path"].iter().any(|k| text_of(p, k).is_some());
    let foreign_thread = text_of(p, "thread_source").is_some_and(|t| t != "user");
    !(subagent_source || child || foreign_thread) && interactive_source(source)
}

/// Superficie normalizada: `cli`, `vscode` o `desktop`.
fn entrypoint(p: &Obj) -> Option<&'static str> {
    let originator = text_of(p, "originator").unwrap_or("").to_ascii_lowercase();
    if originator.contains("desktop") {
        return Some("desktop");
    }
    if originator.contains("vscode") {
        return Some("vscode");
    }
    if originator.contains("cli") {
        return Some("cli");
    }
    match p.get("source").and_then(Value::as_str) {
        Some("cli") => Some("cli"),
        Some("vscode") => Some("vscode"),
        _ => None,
    }
}

fn apply_header(line: &Value, meta: &mut SessionMeta) {
    let Some(p) = line.get("payload").and_then(Value::as_object) else { return };
    meta.hidden = !is_user_thread(p);
    // Sin id en el encabezado, el UUID del nombre del archivo.
    let name_id = meta.file.file_name().and_then(|n| n.to_str()).and_then(thread_id_of);
    meta.id = text_of(p, "id").or(name_id).unwrap_or("sin-id").to_string();
    meta.cwd = text_of(p, "cwd").map(strip_verbatim).filter(|c| !c.is_empty());
    meta.git_branch = p.get("git").and_then(Value::as_object).and_then(|g| text_of(g, "branch")).filter(|b| *b != "HEAD").map(String::from);
    meta.entrypoint = entrypoint(p).map(String::from);
    let ts = |t: Option<&str>| t.and_then(|t| t.parse::<jiff::Timestamp>().ok()).map(|t| t.as_millisecond());
    let line_ts = line.as_object().and_then(|o| text_of(o, "timestamp"));
    meta.started_ms = ts(text_of(p, "timestamp")).or_else(|| ts(line_ts));
}

/// Texto humano de un mensaje: sin el envoltorio del IDE ni contenido de sistema.
fn human_text(content: Content) -> Option<String> {
    let parts = match content {
        Content::Text(t) => vec![Part { kind: Some(Value::from("text")), text: Some(Value::String(t)) }],
        Content::Parts(p) => p,
        Content::Other(_) => return None,
    };
    let mut text = String::new();
    let mut image = false;
    for part in &parts {
        match (part.kind.as_ref().and_then(Value::as_str), part.text.as_ref().and_then(Value::as_str)) {
            (Some("text" | "input_text"), Some(t)) => {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(t);
            }
            (Some(k), _) if k.contains("image") => image = true,
            _ => {}
        }
    }
    let body = match text.find(REQUEST_MARK) {
        Some(i) => &text[i + REQUEST_MARK.len()..],
        // Solo contexto del IDE, sin petición: no hay texto humano.
        None if text.trim_start().starts_with(IDE_PREFIX) => "",
        None => &text,
    };
    let body = body.trim();
    if body.is_empty() {
        return image.then(|| "[imagen]".to_string());
    }
    // No se usa `clean_prompt` (reglas de Claude): descartaría el HTML u otro texto que alguien
    // pegue empezando por `<`. Aquí solo se saltan los envoltorios que Codex inyecta él mismo.
    if SYSTEM_TAGS.iter().any(|t| body.starts_with(t)) {
        return None;
    }
    Some(collapse(body))
}

fn add_prompt(meta: &mut SessionMeta, content: Option<Content>) {
    let Some(p) = content.and_then(human_text) else { return };
    meta.prompts += 1;
    meta.has_messages = true;
    if meta.first_prompt.is_none() {
        meta.first_prompt = Some(p.clone());
    }
    meta.last_prompt = Some(p);
}

// ---------------------------------------------------------------- nombres

struct Names {
    size: u64,
    mtime_ms: i64,
    by_id: HashMap<String, String>,
}

/// Nombres ya leídos por archivo; solo se relee si cambia su tamaño o fecha.
static NAMES: Mutex<BTreeMap<PathBuf, Names>> = Mutex::new(BTreeMap::new());

#[derive(Deserialize)]
struct IndexRow {
    id: Option<String>,
    thread_name: Option<String>,
}

fn read_names(data: &[u8]) -> HashMap<String, String> {
    let mut by_id = HashMap::new();
    for line in data.split(|b| *b == b'\n') {
        if line.len() > MAX_LINE || memchr::memmem::find(line, b"\"thread_name\"").is_none() {
            continue;
        }
        let Ok(row) = serde_json::from_slice::<IndexRow>(line) else { continue };
        if let (Some(id), Some(name)) = (row.id, row.thread_name) {
            let name = collapse(&name);
            // Gana la última línea de cada id; un nombre vacío no borra el anterior.
            if !id.is_empty() && !name.is_empty() {
                by_id.insert(id, name);
            }
        }
    }
    by_id
}

/// Aplica los nombres de `session_index.jsonl` como título. Barato (compara
/// tamaño y fecha del archivo) e idempotente.
pub fn finish(home: &Path, metas: &mut Prev) {
    let path = home.join("session_index.jsonl");
    let mut cache = NAMES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let md = match std::fs::metadata(&path) {
        Ok(md) => md,
        Err(e) => {
            cache.remove(&path);
            if e.kind() != std::io::ErrorKind::NotFound && !WARNED_NAMES.swap(true, Ordering::Relaxed) {
                crate::warn_log!("no se pudo leer {}: {e}", path.display());
            }
            return;
        }
    };
    let (size, mtime) = (md.len(), sessions::mtime_ms(&md));
    if !cache.get(&path).is_some_and(|c| c.size == size && c.mtime_ms == mtime) {
        match std::fs::read(&path) {
            Ok(data) => {
                cache.insert(path.clone(), Names { size, mtime_ms: mtime, by_id: read_names(&data) });
            }
            Err(e) => {
                if !WARNED_NAMES.swap(true, Ordering::Relaxed) {
                    crate::warn_log!("no se pudo leer {}: {e}", path.display());
                }
                // Se sigue con lo último que se pudo leer.
            }
        }
    }
    let Some(names) = cache.get(&path) else { return };
    for m in metas.values_mut() {
        if let Some(name) = names.by_id.get(&m.id)
            && m.ai_title.as_deref() != Some(name)
        {
            m.ai_title = Some(name.clone());
        }
    }
}

// ---------------------------------------------------------------- argumentos

/// Flags de un modo de permisos (los verificados contra `codex --help`).
fn permission_flags(value: &str) -> &'static [&'static str] {
    match value {
        "ask" => &["-a", "on-request", "-s", "workspace-write"],
        "read-only" => &["-s", "read-only"],
        "never-ask" => &["-a", "never", "-s", "workspace-write"],
        "bypass" => &["--dangerously-bypass-approvals-and-sandbox"],
        _ => &[],
    }
}

/// Argumentos de `codex`. El id de una sesión nueva lo genera Codex, y no hay
/// flag para nombrarla; en `resume` y `fork` los flags van tras el subcomando.
pub fn args(mode: &Mode, opts: &LaunchOptions) -> (Vec<String>, Option<String>) {
    let mut a = Vec::new();
    let id = match mode {
        Mode::New => None,
        Mode::Resume(id) => {
            a.extend(["resume".to_string(), id.clone()]);
            Some(id.clone())
        }
        Mode::Fork(id) => {
            a.extend(["fork".to_string(), id.clone()]);
            None
        }
    };
    if let Some(m) = opts.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        a.extend(["--model".to_string(), m.to_string()]);
    }
    if let Some(p) = opts.permission_mode.as_deref() {
        a.extend(permission_flags(p).iter().map(|f| f.to_string()));
    }
    (a, id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::sync::atomic::AtomicU32;

    const ID1: &str = "01900000-0000-7000-8000-000000000001";
    const ID2: &str = "01900000-0000-7000-8000-000000000002";
    const ID3: &str = "01900000-0000-7000-8000-000000000003";
    const ID4: &str = "01900000-0000-7000-8000-000000000004";

    /// Carpeta temporal única que se borra al soltarla.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str) -> Tmp {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join("claunch-tests").join(format!("codex-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Tmp(dir)
        }

        fn write(&self, rel: &str, content: &str) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, content).unwrap();
            p
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rollout_name(id: &str) -> String {
        format!("rollout-2026-07-16T09-17-40-{id}.jsonl")
    }

    fn line(ts: &str, kind: &str, payload: Value) -> String {
        json!({"timestamp": ts, "ordinal": 1, "type": kind, "payload": payload}).to_string()
    }

    fn meta_line(id: &str, extra: Value) -> String {
        let mut p = json!({
            "session_id": id, "id": id, "timestamp": "2026-07-16T13:17:40.513Z", "cwd": r"c:\Users\demo\Desktop\proyecto-a",
            "originator": "codex_vscode", "cli_version": "0.144.5", "source": "vscode", "thread_source": "user",
            "model_provider": "openai", "base_instructions": {"text": "(texto inventado)"}, "history_mode": "paginated",
            "git": {"commit_hash": "0123", "branch": "main", "repository_url": "https://example.invalid/demo.git"}
        });
        for (k, v) in extra.as_object().unwrap() {
            if v.is_null() {
                p.as_object_mut().unwrap().remove(k);
            } else {
                p[k] = v.clone();
            }
        }
        line("2026-07-16T13:17:42.222Z", "session_meta", p)
    }

    fn user(text: &str) -> String {
        line(
            "2026-07-16T13:17:50.200Z",
            "event_msg",
            json!({"type": "item_completed", "item": {"type": "UserMessage", "id": "i", "content": [{"type": "text", "text": text, "text_elements": []}]}}),
        )
    }

    fn agent(text: &str) -> String {
        line("2026-07-16T13:18:10.000Z", "event_msg", json!({"type": "item_completed", "item": {"type": "AgentMessage", "content": [{"type": "text", "text": text}]}}))
    }

    fn event(kind: &str) -> String {
        line("2026-07-16T13:17:50.000Z", "event_msg", json!({"type": kind, "turn_id": "t1"}))
    }

    fn turn_context(model: &str) -> String {
        line("2026-07-16T13:17:50.100Z", "turn_context", json!({"turn_id": "t1", "cwd": r"C:\Users\demo\Desktop\proyecto-a", "model": model, "effort": "high"}))
    }

    fn index_one(p: &Path, prev: Option<&SessionMeta>) -> SessionMeta {
        let mut prev_map = Prev::new();
        if let Some(m) = prev {
            prev_map.insert(p.to_path_buf(), m.clone());
        }
        let mut out = index(Path::new("."), p, &prev_map);
        assert_eq!(out.len(), 1);
        out.pop().unwrap().1
    }

    fn index_lines(t: &Tmp, id: &str, lines: &[String]) -> SessionMeta {
        let p = t.write(&format!("sessions/2026/07/16/{}", rollout_name(id)), &(lines.join("\n") + "\n"));
        index_one(&p, None)
    }

    // ---- unidades

    #[test]
    fn rollout_names() {
        assert_eq!(thread_id_of(&rollout_name(ID1)), Some(ID1));
        // Hilo revertido: dos UUID separados por `_`; el del hilo es el primero.
        assert_eq!(thread_id_of(&format!("rollout-2026-07-16T09-17-40-{ID1}_{ID2}.jsonl")), Some(ID1));
        for bad in [
            "rollout-2026-07-16T09-17-40-corto.jsonl",
            &format!("rollout-2026-07-16T09-17-40-{ID1}.jsonl.zst"),
            &format!("rollout-2026-07-16T09-17-40-{ID1}_x.jsonl"),
            &format!("sesion-2026-07-16T09-17-40-{ID1}.jsonl"),
            &format!("rollout-2026-07-16T09-17-40-{ID1}.json"),
            "rollout-é.jsonl",
        ] {
            assert_eq!(thread_id_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn units_walk_sessions_only_and_dedupe_threads() {
        let t = Tmp::new("units");
        let a = t.write(&format!("sessions/2026/07/16/{}", rollout_name(ID1)), "{}\n");
        let b = t.write(&format!("sessions/2026/08/01/{}", rollout_name(ID2)), "{}\n");
        t.write(&format!("archived_sessions/{}", rollout_name(ID3)), "{}\n");
        t.write(&format!("sessions/2026/07/16/rollout-2026-07-16T09-17-40-{ID4}.jsonl.zst"), "x");
        t.write("sessions/2026/07/16/notas.txt", "x");
        t.write("session_index.jsonl", "");
        let mut first = units(&t.0);
        first.sort();
        assert_eq!(first, [a, b]);

        // Un hilo revertido deja dos rollouts: solo cuenta el más reciente.
        let old = t.write(&format!("sessions/2026/09/01/{}", rollout_name(ID3)), "{}\n");
        let new = t.write(&format!("sessions/2026/09/02/rollout-2026-09-02T10-00-00-{ID3}_{ID4}.jsonl"), "{}\n{}\n");
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000)).unwrap();
        drop(f);
        let all = units(&t.0);
        assert_eq!(all.len(), 3);
        assert!(all.contains(&new) && !all.contains(&old));
    }

    #[test]
    fn units_of_missing_home_is_empty() {
        assert!(units(&std::env::temp_dir().join("claunch-tests").join("codex-no-existe")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn units_do_not_follow_symlinks() {
        let t = Tmp::new("symlink");
        let outside = t.write(&format!("fuera/{}", rollout_name(ID1)), "{}\n");
        std::fs::create_dir_all(t.0.join("sessions")).unwrap();
        std::os::unix::fs::symlink(t.0.join("fuera"), t.0.join("sessions/enlace")).unwrap();
        std::os::unix::fs::symlink(&outside, t.0.join("sessions").join(rollout_name(ID2))).unwrap();
        assert!(units(&t.0).is_empty());
    }

    // ---- registros

    #[test]
    fn parses_a_full_user_rollout() {
        let t = Tmp::new("full");
        let m = index_lines(
            &t,
            ID1,
            &[
                meta_line(ID1, json!({})),
                event("task_started"),
                turn_context("gpt-5.5"),
                user("# Context from my IDE setup:\n\n## Active file: src/main.rs\n\n## My request for Codex:\nHaz un resumen ficticio"),
                agent("respuesta ficticia"),
                event("task_complete"),
            ],
        );
        assert_eq!(m.harness, Harness::Codex);
        assert_eq!(m.id, ID1);
        assert_eq!(m.cwd.as_deref(), Some(r"c:\Users\demo\Desktop\proyecto-a"));
        assert_eq!(m.git_branch.as_deref(), Some("main"));
        assert_eq!(m.entrypoint.as_deref(), Some("vscode"));
        assert_eq!(m.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(m.prompts, 1);
        assert_eq!(m.first_prompt.as_deref(), Some("Haz un resumen ficticio"));
        assert_eq!(m.last_prompt.as_deref(), Some("Haz un resumen ficticio"));
        assert!(m.has_messages && !m.hidden && !m.busy);
        assert_eq!(m.started_ms, Some(1_784_207_860_513));
        assert!(m.size > 0 && m.mtime_ms > 0);
    }

    #[test]
    fn old_header_without_git_or_thread_source_is_listed() {
        let t = Tmp::new("old");
        let m = index_lines(
            &t,
            ID2,
            &[meta_line(ID2, json!({"git": null, "thread_source": null, "cli_version": "0.115.0-alpha.11"})), user("hola")],
        );
        assert!(!m.hidden && m.has_messages);
        assert_eq!(m.git_branch, None);
        assert_eq!(m.first_prompt.as_deref(), Some("hola"));
    }

    #[test]
    fn exclusions_follow_the_spec() {
        let t = Tmp::new("excl");
        let sub = json!({"subagent": {"thread_spawn": {"parent_thread_id": ID1, "depth": 1, "agent_path": "/root/w", "agent_nickname": "Ada", "agent_role": null}}});
        let cases: Vec<(&str, Value, bool)> = vec![
            ("vscode", json!({}), false),
            ("cli", json!({"source": "cli", "originator": "codex_cli_rs", "thread_source": null}), false),
            ("sin source", json!({"source": null}), false),
            ("fork de usuario", json!({"forked_from_id": ID1}), false),
            ("custom atlas", json!({"source": {"custom": "atlas"}}), false),
            ("custom chatgpt", json!({"source": {"custom": "chatgpt"}}), false),
            ("thread_spawn", json!({"source": sub, "thread_source": "subagent", "parent_thread_id": ID1, "agent_nickname": "Ada"}), true),
            ("solo parent_thread_id", json!({"parent_thread_id": ID1}), true),
            ("solo agent_path", json!({"agent_path": "/root/x"}), true),
            ("revisión", json!({"source": {"subagent": "review"}}), true),
            ("guardian antiguo", json!({"source": {"subagent": {"other": "guardian"}}, "thread_source": "subagent"}), true),
            ("guardian nuevo", json!({"source": {"internal": "guardian"}}), true),
            ("thread_source guardian", json!({"thread_source": "guardian_review"}), true),
            ("memoria", json!({"thread_source": "memory_consolidation"}), true),
            ("thread_source desconocido", json!({"thread_source": "otra-cosa"}), true),
            ("exec", json!({"source": "exec", "originator": "codex_exec"}), true),
            ("mcp", json!({"source": "mcp"}), true),
            ("custom otro", json!({"source": {"custom": "otro"}}), true),
            ("unknown", json!({"source": "weird"}), true),
        ];
        for (name, extra, hidden) in cases {
            let m = index_lines(&t, ID1, &[meta_line(ID1, extra), user("hola")]);
            assert_eq!(m.hidden, hidden, "{name}");
            // Se devuelven igualmente, para que el índice no las relea.
            assert!(m.has_messages || hidden, "{name}");
        }
    }

    #[test]
    fn hidden_threads_stay_hidden_and_stop_parsing() {
        let t = Tmp::new("hidden");
        let m = index_lines(&t, ID3, &[meta_line(ID3, json!({"source": "exec"})), user("hola"), event("task_started")]);
        assert!(m.hidden && m.prompts == 0 && !m.busy);
    }

    #[test]
    fn entrypoints() {
        let t = Tmp::new("entry");
        for (extra, want) in [
            (json!({"originator": "Codex Desktop"}), Some("desktop")),
            (json!({"originator": "codex_vscode"}), Some("vscode")),
            (json!({"originator": "codex_cli_rs", "source": "cli"}), Some("cli")),
            (json!({"originator": null, "source": "cli"}), Some("cli")),
            (json!({"originator": null, "source": {"custom": "atlas"}}), None),
        ] {
            let m = index_lines(&t, ID1, &[meta_line(ID1, extra), user("hola")]);
            assert_eq!(m.entrypoint.as_deref(), want);
        }
    }

    #[test]
    fn prompts_are_human_only() {
        let t = Tmp::new("prompts");
        let image_only = line("2026-07-16T13:17:51.000Z", "event_msg", json!({"type": "item_completed", "item": {"type": "UserMessage", "content": [{"type": "image", "image_url": "data:x"}]}}));
        let legacy = line("2026-07-16T13:17:52.000Z", "event_msg", json!({"type": "user_message", "message": "variante   antigua"}));
        let m = index_lines(
            &t,
            ID1,
            &[
                meta_line(ID1, json!({})),
                user("<environment_context>sistema</environment_context>"),
                user("# Context from my IDE setup:\n## Active file: a.rs"),
                user("   "),
                agent("no cuenta"),
                line("2026-07-16T13:17:50.000Z", "response_item", json!({"type": "message", "role": "user", "content": [{"type": "input_text", "text": "contexto inyectado"}]})),
                user("primero"),
                image_only,
                legacy,
                user("# Files mentioned by the user:\n## a.rs\n\n## My request for Codex:\nfinal\n\ncon saltos"),
            ],
        );
        assert_eq!(m.prompts, 4);
        assert_eq!(m.first_prompt.as_deref(), Some("primero"));
        assert_eq!(m.last_prompt.as_deref(), Some("final con saltos"));
        let only_images = index_lines(&t, ID2, &[meta_line(ID2, json!({})), image_only_line()]);
        assert_eq!((only_images.prompts, only_images.first_prompt.as_deref()), (1, Some("[imagen]")));
        // Sin ningún mensaje humano no se lista.
        let empty = index_lines(&t, ID3, &[meta_line(ID3, json!({})), event("task_started"), agent("hola")]);
        assert!(!empty.has_messages && empty.prompts == 0);
    }

    fn image_only_line() -> String {
        line("2026-07-16T13:17:51.000Z", "event_msg", json!({"type": "item_completed", "item": {"type": "UserMessage", "content": [{"type": "image"}]}}))
    }

    #[test]
    fn busy_follows_the_last_turn_event() {
        let t = Tmp::new("busy");
        let head = meta_line(ID1, json!({}));
        let busy = |events: &[&str]| {
            let mut lines = vec![head.clone(), user("hola")];
            lines.extend(events.iter().map(|e| event(e)));
            index_lines(&t, ID1, &lines).busy
        };
        assert!(busy(&["task_started"]));
        assert!(!busy(&["task_started", "task_complete"]));
        assert!(!busy(&["task_started", "turn_aborted"]));
        assert!(busy(&["task_started", "task_complete", "task_started"]));
        assert!(!busy(&[]));
    }

    #[test]
    fn model_is_the_last_one_seen() {
        let t = Tmp::new("model");
        let settings = line("2026-07-20T14:30:06.815Z", "event_msg", json!({"type": "thread_settings_applied", "thread_settings": {"model": "gpt-5.6-sol", "cwd": "x"}}));
        let m = index_lines(&t, ID1, &[meta_line(ID1, json!({})), turn_context("gpt-5.5"), user("a"), settings.clone()]);
        assert_eq!(m.model.as_deref(), Some("gpt-5.6-sol"));
        let m = index_lines(&t, ID1, &[meta_line(ID1, json!({})), settings, turn_context("gpt-5.4")]);
        assert_eq!(m.model.as_deref(), Some("gpt-5.4"));
        // Sin modelo no se inventa.
        assert_eq!(index_lines(&t, ID1, &[meta_line(ID1, json!({})), user("a")]).model, None);
    }

    #[test]
    fn cwd_is_cleaned_and_falls_back_to_turn_context() {
        assert_eq!(strip_verbatim(r"\\?\C:\Users\demo"), r"C:\Users\demo");
        assert_eq!(strip_verbatim(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
        assert_eq!(strip_verbatim("/home/demo/app"), "/home/demo/app");
        let t = Tmp::new("cwd");
        let m = index_lines(&t, ID1, &[meta_line(ID1, json!({"cwd": r"\\?\C:\Users\demo\app"})), user("a")]);
        assert_eq!(m.cwd.as_deref(), Some(r"C:\Users\demo\app"));
        let m = index_lines(&t, ID1, &[meta_line(ID1, json!({"cwd": null})), turn_context("m"), user("a")]);
        assert_eq!(m.cwd.as_deref(), Some(r"C:\Users\demo\Desktop\proyecto-a"));
    }

    #[test]
    fn id_falls_back_to_the_file_name() {
        let t = Tmp::new("id");
        let m = index_lines(&t, ID4, &[meta_line(ID4, json!({"id": null})).replace(r#""type":"session_meta""#, r#""type":"otra""#), user("hola")]);
        assert_eq!(m.id, ID4);
        let m = index_lines(&t, ID4, &[meta_line(ID4, json!({})), user("hola")]);
        assert_eq!(m.id, ID4);
    }

    // ---- incremental y robustez

    #[test]
    fn incremental_equals_full_when_lines_are_appended() {
        let t = Tmp::new("inc");
        let first = [meta_line(ID1, json!({})), event("task_started"), user("uno"), agent("a")];
        let p = t.write(&format!("sessions/2026/07/16/{}", rollout_name(ID1)), &(first.join("\n") + "\n"));
        let m1 = index_one(&p, None);
        assert!(m1.busy && m1.prompts == 1);
        // Sin cambios: lo mismo.
        assert_eq!(index_one(&p, Some(&m1)), m1);

        let more = [event("task_complete"), turn_context("gpt-5.5"), user("dos"), user("tres"), event("task_started")];
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        // La última línea queda a medias y se completa después.
        let tail = more.join("\n");
        write!(f, "{}", &tail[..tail.len() - 10]).unwrap();
        drop(f);
        let mut stale = m1.clone();
        stale.mtime_ms -= 1;
        let mid = index_one(&p, Some(&stale));
        assert!(mid.indexed_to < std::fs::metadata(&p).unwrap().len());

        let mut f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        // Reescribe el final completo (equivale a que Codex terminó de escribir la línea).
        let full_text = first.join("\n") + "\n" + &more.join("\n") + "\n";
        f.write_all(full_text.as_bytes()).unwrap();
        drop(f);
        let mut stale = mid.clone();
        stale.mtime_ms -= 1;
        let inc = index_one(&p, Some(&stale));
        let full = index_one(&p, None);
        assert_eq!(inc, full);
        assert_eq!((full.prompts, full.last_prompt.as_deref(), full.busy), (3, Some("tres"), true));
        assert_eq!(full.model.as_deref(), Some("gpt-5.5"));
    }

    #[test]
    fn malformed_truncated_and_huge_lines_are_skipped() {
        let t = Tmp::new("bad");
        let huge = line("2026-07-16T13:17:55.000Z", "response_item", json!({"type": "custom_tool_call_output", "output": "x".repeat(MAX_LINE + 1000)}));
        let huge_user = user(&"y".repeat(MAX_LINE + 10));
        let lines = [
            "no es json".to_string(),
            "{".to_string(),
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"cortado"#.to_string(),
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":"UserMessage"}}"#.to_string(),
            r#"{"timestamp":"x","type":"session_meta","payload":"roto"}"#.to_string(),
            meta_line(ID1, json!({})),
            huge,
            huge_user,
            r#"{"type":"turn_context","payload":{"model":42}}"#.to_string(),
            user("sobrevive"),
        ];
        let m = index_lines(&t, ID1, &lines);
        assert_eq!((m.prompts, m.first_prompt.as_deref(), m.id.as_str()), (1, Some("sobrevive"), ID1));
        assert_eq!(m.model, None);
        assert!(!m.hidden);

        // Un archivo vacío o sin encabezado no se lista, pero tampoco falla.
        let p = t.write(&format!("sessions/2026/07/16/{}", rollout_name(ID2)), "");
        let m = index_one(&p, None);
        assert!(!m.has_messages);
        assert_eq!(m.id, ID2);
    }

    #[test]
    fn unreadable_file_keeps_the_previous_entry() {
        let t = Tmp::new("gone");
        let p = t.write(&format!("sessions/2026/07/16/{}", rollout_name(ID1)), &format!("{}\n{}\n", meta_line(ID1, json!({})), user("hola")));
        let m = index_one(&p, None);
        std::fs::remove_file(&p).unwrap();
        assert_eq!(index_one(&p, Some(&m)), m);
        assert!(index(Path::new("."), &p, &Prev::new()).is_empty());
    }

    // ---- títulos

    fn meta_with(id: &str) -> SessionMeta {
        SessionMeta { harness: Harness::Codex, id: id.to_string(), has_messages: true, ..Default::default() }
    }

    #[test]
    fn thread_names_become_titles_and_the_last_wins() {
        let t = Tmp::new("names");
        t.write(
            "session_index.jsonl",
            &[
                json!({"id": ID1, "thread_name": "Resumen ficticio inicial", "updated_at": "2026-07-16T13:20:00.1234567Z"}).to_string(),
                "basura".to_string(),
                json!({"id": ID1, "thread_name": "Nombre tras renombrar", "updated_at": "2026-07-18T08:00:00.0000000Z"}).to_string(),
                json!({"id": ID2, "thread_name": "  varias \n palabras  "}).to_string(),
                json!({"id": ID2, "thread_name": "   "}).to_string(),
                json!({"id": ID3, "otro": 1}).to_string(),
            ]
            .join("\n"),
        );
        let mut metas = Prev::new();
        for id in [ID1, ID2, ID3, ID4] {
            metas.insert(PathBuf::from(id), meta_with(id));
        }
        metas.get_mut(Path::new(ID4)).unwrap().first_prompt = Some("primer prompt".into());
        finish(&t.0, &mut metas);
        let title = |id: &str| metas[Path::new(id)].title().to_string();
        assert_eq!(title(ID1), "Nombre tras renombrar");
        assert_eq!(title(ID2), "varias palabras");
        assert_eq!(title(ID3), "(sin título)");
        assert_eq!(title(ID4), "primer prompt");
        // Idempotente: aplicarlo otra vez no cambia nada.
        let before = metas.clone();
        finish(&t.0, &mut metas);
        assert_eq!(metas, before);
    }

    #[test]
    fn name_cache_follows_the_file_and_survives_its_absence() {
        let t = Tmp::new("cache");
        let mut metas = Prev::new();
        metas.insert(PathBuf::from(ID1), meta_with(ID1));
        // Sin archivo no pasa nada.
        finish(&t.0, &mut metas);
        assert_eq!(metas[Path::new(ID1)].ai_title, None);
        let idx = t.write("session_index.jsonl", &json!({"id": ID1, "thread_name": "uno"}).to_string());
        finish(&t.0, &mut metas);
        assert_eq!(metas[Path::new(ID1)].ai_title.as_deref(), Some("uno"));
        // Cambia el tamaño: se relee.
        let mut f = std::fs::OpenOptions::new().append(true).open(&idx).unwrap();
        write!(f, "\n{}", json!({"id": ID1, "thread_name": "dos"})).unwrap();
        drop(f);
        finish(&t.0, &mut metas);
        assert_eq!(metas[Path::new(ID1)].ai_title.as_deref(), Some("dos"));
    }

    // ---- adversariales

    #[test]
    fn header_with_odd_types_is_still_read_and_classified() {
        let t = Tmp::new("odd");
        // Tipos raros en campos que no interesan no pierden el encabezado.
        let m = index_lines(&t, ID1, &[meta_line(ID1, json!({"git": "roto", "agent_path": 5, "dynamic_tools": [1, {"a": null}], "cli_version": 7})), user("hola")]);
        assert!(!m.hidden && m.cwd.is_some() && m.started_ms.is_some() && m.git_branch.is_none());
        // Un subagente con tipos raros sigue oculto.
        let m = index_lines(&t, ID2, &[meta_line(ID2, json!({"source": {"subagent": 3}, "git": 1})), user("hola")]);
        assert!(m.hidden);
        // `source` de un tipo imposible = no interactiva.
        assert!(index_lines(&t, ID3, &[meta_line(ID3, json!({"source": 7})), user("hola")]).hidden);
    }

    #[test]
    fn a_second_session_meta_never_overrides_the_header() {
        let t = Tmp::new("twometa");
        // Sin marca de tiempo ni id en el encabezado: el segundo (copiado de un padre) tampoco cuenta.
        let first = meta_line(ID1, json!({"id": null, "timestamp": null, "cwd": "/a"}));
        let parent = meta_line(ID2, json!({"cwd": "/b", "source": "exec"}));
        let m = index_lines(&t, ID1, &[first, parent, user("hola")]);
        assert_eq!((m.id.as_str(), m.cwd.as_deref(), m.hidden), (ID1, Some("/a"), false));
    }

    #[test]
    fn odd_cwds_are_kept_as_recorded() {
        let t = Tmp::new("cwds");
        for (raw, want) in [
            (r"\\?\UNC\srv\share\proyecto", r"\\srv\share\proyecto"),
            (r"C:\Users\demo\Mis proyectos\año ñandú", r"C:\Users\demo\Mis proyectos\año ñandú"),
            ("/home/demo/proyecto/", "/home/demo/proyecto/"),
            ("  /home/demo/x  ", "/home/demo/x"),
        ] {
            let m = index_lines(&t, ID1, &[meta_line(ID1, json!({ "cwd": raw })), user("a")]);
            assert_eq!(m.cwd.as_deref(), Some(want));
        }
        for bad in [json!(""), json!("   "), json!(5), Value::Null] {
            let m = index_lines(&t, ID1, &[meta_line(ID1, json!({ "cwd": bad })), user("a")]);
            assert_eq!(m.cwd, None);
        }
    }

    #[test]
    fn user_message_content_variants() {
        let t = Tmp::new("content");
        let raw = |content: Value| line("2026-07-16T13:17:50.200Z", "event_msg", json!({"type": "item_completed", "item": {"type": "UserMessage", "content": content}}));
        let m = index_lines(
            &t,
            ID1,
            &[
                meta_line(ID1, json!({})),
                raw(json!("texto suelto")),
                raw(json!(null)),
                raw(json!(12)),
                raw(json!([{"type": 3, "text": "x"}, {"text": "sin tipo"}, "cadena", {"type": "text", "text": 9}])),
                raw(json!([{"type": "input_text", "text": "parte "}, {"type": "text", "text": "dos"}])),
            ],
        );
        assert_eq!((m.prompts, m.first_prompt.as_deref(), m.last_prompt.as_deref()), (2, Some("texto suelto"), Some("parte dos")));
    }

    #[test]
    fn pasted_markup_is_a_human_prompt() {
        let t = Tmp::new("markup");
        // Observado con datos reales: HTML pegado por la persona (empieza por `<`) es un prompt.
        let m = index_lines(&t, ID1, &[meta_line(ID1, json!({})), user("<header class=\"a\">hola</header> arréglalo"), user("[Request interrupted] sigue"), user("<turn_aborted>x</turn_aborted>")]);
        assert_eq!((m.prompts, m.first_prompt.as_deref()), (2, Some("<header class=\"a\">hola</header> arréglalo")));
    }

    #[test]
    fn crlf_and_blank_lines_are_tolerated() {
        let t = Tmp::new("crlf");
        let text = [meta_line(ID1, json!({})), String::new(), user("hola"), "   ".into(), event("task_started")].join("\r\n") + "\r\n";
        let p = t.write(&format!("sessions/2026/07/16/{}", rollout_name(ID1)), &text);
        let m = index_one(&p, None);
        assert_eq!((m.prompts, m.busy, m.id.as_str()), (1, true, ID1));
    }

    #[test]
    fn chunked_appends_equal_a_full_read() {
        let t = Tmp::new("chunks");
        let lines = [
            meta_line(ID1, json!({})),
            event("task_started"),
            turn_context("gpt-5.5"),
            user("uno"),
            "basura".to_string(),
            agent("a"),
            event("task_complete"),
            user("# Context from my IDE setup:\n## My request for Codex:\ndos"),
            event("task_started"),
        ];
        let text = lines.join("\n") + "\n";
        let p = t.write(&format!("sessions/2026/07/16/{}", rollout_name(ID1)), "");
        let mut full = {
            let q = t.write(&format!("sessions/2026/07/16/{}", rollout_name(ID2)), &text);
            index_one(&q, None)
        };
        full.file = p.clone();
        // Troceado cada `step` bytes (parte líneas por la mitad).
        for step in [97usize, 333, 1000] {
            std::fs::write(&p, "").unwrap();
            let mut prev: Option<SessionMeta> = None;
            let bytes = text.as_bytes();
            let (mut at, mut tick) = (0, 0i64);
            while at < bytes.len() {
                let end = (at + step).min(bytes.len());
                let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
                f.write_all(&bytes[at..end]).unwrap();
                drop(f);
                at = end;
                tick += 1;
                let mut m = index_one(&p, prev.as_ref());
                // La fecha del sistema de archivos puede no cambiar entre trozos: se fuerza.
                m.mtime_ms += tick;
                prev = Some(m);
            }
            let mut got = index_one(&p, prev.as_ref());
            got.mtime_ms = full.mtime_ms;
            assert_eq!(got, full, "step {step}");
        }
    }

    #[test]
    fn second_pass_is_identical_and_titles_are_stable() {
        let t = Tmp::new("stable");
        let p = t.write(&format!("sessions/2026/07/16/{}", rollout_name(ID1)), &([meta_line(ID1, json!({})), user("hola")].join("\n") + "\n"));
        t.write("session_index.jsonl", &json!({"id": ID1, "thread_name": "Nombre"}).to_string());
        let mut prev = Prev::new();
        let mut snapshots = Vec::new();
        for _ in 0..3 {
            let mut next = Prev::new();
            for u in units(&t.0) {
                for (k, m) in index(&t.0, &u, &prev) {
                    next.insert(k, m);
                }
            }
            finish(&t.0, &mut next);
            snapshots.push(next.clone());
            prev = next;
        }
        assert_eq!(snapshots[0], snapshots[1]);
        assert_eq!(snapshots[1], snapshots[2]);
        assert_eq!(snapshots[2][&p].ai_title.as_deref(), Some("Nombre"));
    }

    #[test]
    fn duplicated_thread_ids_in_different_files_are_both_indexed() {
        let t = Tmp::new("dups");
        // Un rollout copiado a mano con otro nombre: el catálogo decide cuál gana por id.
        let a = t.write(&format!("sessions/2026/07/16/{}", rollout_name(ID1)), &([meta_line(ID1, json!({})), user("a")].join("\n") + "\n"));
        let b = t.write(&format!("sessions/2026/07/17/rollout-2026-07-17T09-17-40-{ID2}.jsonl"), &([meta_line(ID1, json!({})), user("b")].join("\n") + "\n"));
        assert_eq!(units(&t.0).len(), 2);
        assert_eq!(index_one(&a, None).id, index_one(&b, None).id);
    }

    #[test]
    fn name_index_edge_cases() {
        let t = Tmp::new("names2");
        let huge = json!({"id": ID1, "thread_name": "x".repeat(MAX_LINE + 10)}).to_string();
        t.write(
            "session_index.jsonl",
            &[
                json!({"id": ID1, "thread_name": "bueno"}).to_string(),
                huge,
                json!({"id": 5, "thread_name": "numérico"}).to_string(),
                json!({"id": ID2, "thread_name": 7}).to_string(),
                "{\"id\":\"x\",\"thread_name\":\"cortado".to_string(),
                json!({"id": ID3, "thread_name": "x".repeat(500)}).to_string(),
            ]
            .join("\r\n"),
        );
        let mut metas = Prev::new();
        for id in [ID1, ID2, ID3] {
            metas.insert(PathBuf::from(id), meta_with(id));
        }
        finish(&t.0, &mut metas);
        assert_eq!(metas[Path::new(ID1)].ai_title.as_deref(), Some("bueno"));
        assert_eq!(metas[Path::new(ID2)].ai_title, None);
        assert!(metas[Path::new(ID3)].ai_title.as_deref().is_some_and(|n| n.chars().count() < 300));
    }

    // ---- argumentos

    fn opts(model: Option<&str>, perm: Option<&str>) -> LaunchOptions {
        LaunchOptions { model: model.map(String::from), permission_mode: perm.map(String::from), ..Default::default() }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn args_for_new_resume_and_fork() {
        assert_eq!(args(&Mode::New, &opts(None, None)), (vec![], None));
        assert_eq!(args(&Mode::New, &opts(Some("gpt-5.5"), Some("ask"))), (s(&["--model", "gpt-5.5", "-a", "on-request", "-s", "workspace-write"]), None));
        assert_eq!(args(&Mode::Resume("abc".into()), &opts(None, None)), (s(&["resume", "abc"]), Some("abc".into())));
        assert_eq!(
            args(&Mode::Resume("abc".into()), &opts(Some(" gpt-5.4 "), Some("read-only"))),
            (s(&["resume", "abc", "--model", "gpt-5.4", "-s", "read-only"]), Some("abc".into()))
        );
        assert_eq!(args(&Mode::Fork("abc".into()), &opts(Some("m"), Some("bypass"))), (s(&["fork", "abc", "--model", "m", "--dangerously-bypass-approvals-and-sandbox"]), None));
    }

    #[test]
    fn name_and_worktree_are_ignored() {
        let o = LaunchOptions { name: Some("x".into()), worktree: Some("w".into()), ..Default::default() };
        assert_eq!(args(&Mode::New, &o).0, Vec::<String>::new());
    }

    #[test]
    fn every_permission_maps_to_flags_and_unknown_is_ignored() {
        for p in INFO.permissions {
            let (a, _) = args(&Mode::New, &opts(None, Some(p.value)));
            assert_eq!(a.is_empty(), p.value.is_empty(), "{}", p.value);
            assert!(p.value.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'), "{}", p.value);
            assert!(p.value.is_empty() || !p.hint.is_empty());
        }
        assert_eq!(args(&Mode::New, &opts(None, Some("acceptEdits"))).0, Vec::<String>::new());
        assert_eq!(args(&Mode::New, &opts(Some(""), Some(""))).0, Vec::<String>::new());
        assert_eq!(args(&Mode::New, &opts(None, Some("never-ask"))).0, s(&["-a", "never", "-s", "workspace-write"]));
        // `--approve-for-me` no sale en `--help` y los Codex anteriores a 0.170 lo rechazan: no se ofrece.
        assert_eq!(args(&Mode::New, &opts(None, Some("auto-review"))).0, Vec::<String>::new());
        // `--full-auto` ya no existe en Codex.
        assert!(INFO.permissions.iter().all(|p| !permission_flags(p.value).contains(&"--full-auto")));
    }

    #[test]
    fn info_declares_only_what_codex_supports() {
        assert_eq!((INFO.fork, INFO.name, INFO.worktree), (true, false, false));
        assert!(matches!(INFO.model, ModelInput::Free(_)));
    }

    // ---- ejecutable

    #[test]
    fn locate_picks_the_newest_executable() {
        let t = Tmp::new("locate");
        let local = t.0.join("local");
        let user = t.0.join("user");
        let chome = t.0.join("chome");
        assert_eq!(locate_in(Some(&local), &user, &chome), None);

        // Extensión de VS Code (con una plataforma que no es la nuestra).
        let plat = if cfg!(windows) { "windows-x86_64" } else { "linux-x86_64" };
        let other = if cfg!(windows) { "linux-x86_64" } else { "windows-x86_64" };
        let ext = t.write(&format!("user/.vscode/extensions/openai.chatgpt-26.1106/bin/{plat}/{EXE}"), "x");
        t.write(&format!("user/.vscode/extensions/openai.chatgpt-26.1106/bin/{other}/{EXE}"), "x");
        t.write(&format!("user/.vscode/extensions/otra.extension-1/bin/{plat}/{EXE}"), "x");
        assert_eq!(locate_in(Some(&local), &user, &chome), Some(ext.clone()));

        // Entre la app de escritorio y la extensión gana el ejecutable más reciente.
        let age = |p: &Path, secs: u64| {
            let f = std::fs::File::options().write(true).open(p).unwrap();
            f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs)).unwrap();
        };
        let old = t.write(&format!("local/OpenAI/Codex/bin/aaaa/{EXE}"), "x");
        let new = t.write(&format!("local/OpenAI/Codex/bin/bbbb/{EXE}"), "x");
        age(&old, 1_000);
        age(&ext, 2_000);
        age(&new, 3_000);
        assert_eq!(locate_in(Some(&local), &user, &chome), Some(new));
        // Una extensión más nueva que la app de escritorio pasa delante.
        age(&ext, 4_000);
        assert_eq!(locate_in(Some(&local), &user, &chome), Some(ext.clone()));
        assert_eq!(locate_in(None, &user, &chome), Some(ext));

        // Instalación gestionada en CODEX_HOME.
        let t2 = Tmp::new("locate2");
        let managed = t2.write(&format!("chome/packages/app-server-daemon/current/bin/{EXE}"), "x");
        assert_eq!(locate_in(None, &t2.0.join("user"), &t2.0.join("chome")), Some(managed));
    }
}
