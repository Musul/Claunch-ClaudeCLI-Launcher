//! Pi coding agent: sesiones en `~/.pi/agent/sessions/--<cwd>--/<fecha>_<id>.jsonl`.
//!
//! Cada sesión es un `.jsonl` que solo crece por el final: la primera línea
//! parseable es la cabecera (`type: "session"`) y el resto son entradas
//! (`message`, `session_info`, `model_change`…). Pi solo escribe mensajes
//! completos (nunca parciales de streaming), así que la última entrada dice si
//! el turno quedó a medias. No hay SQLite ni índice propio: se lee el archivo.

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::de::IgnoredAny;
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

use super::{Harness, Info, ModelInput, Perm, Prev};
use crate::config::LaunchOptions;
use crate::data::sessions::{self, SessionMeta, collapse, set_text};
use crate::launch::Mode;

pub const INFO: Info = Info {
    id: "pi",
    label: "Pi",
    tag: "pi",
    program: "pi",
    install: "npm install -g @earendil-works/pi-coding-agent",
    color: "#B997E0",
    fork: true,
    name: true,
    worktree: false,
    model: ModelInput::Free("anthropic/claude-sonnet-5-5"),
    // Pi no tiene permisos interactivos ni sandbox: son perfiles de herramientas.
    permissions: &[
        Perm { value: "", label: "completo", hint: "read, bash, edit y write" },
        Perm { value: READ_ONLY, label: "solo lectura", hint: "solo read, grep, find y ls" },
        Perm { value: NO_TOOLS, label: "sin herramientas", hint: "solo conversa, sin tocar nada" },
    ],
};

/// Perfil «solo lectura» y las herramientas que deja activas.
const READ_ONLY: &str = "read-only";
const READ_ONLY_TOOLS: &str = "read,grep,find,ls";
const NO_TOOLS: &str = "no-tools";

/// Más allá de esto una línea se salta sin leerla (salidas de herramientas, imágenes).
const MAX_LINE: usize = 8 << 20;
/// Pi no reconoce como sesión un archivo cuya cabecera pase de 1 MiB.
const MAX_HEADER: usize = 1 << 20;
/// Las líneas mayores que esto solo se parsean si parecen de un tipo que interesa.
const BIG_LINE: usize = 64 << 10;
/// Los `settings.json` más grandes que esto no se leen.
const MAX_SETTINGS: u64 = 1 << 20;

/// Carpeta de datos (`PI_CODING_AGENT_DIR` o `~/.pi/agent`); admite `~`.
pub fn home() -> PathBuf {
    match std::env::var_os("PI_CODING_AGENT_DIR") {
        Some(dir) if !dir.is_empty() => expand_tilde(&dir),
        _ => crate::paths::home().join(".pi").join("agent"),
    }
}

/// Expande `~` y `~/…` como hace Pi.
fn expand_tilde(raw: &OsStr) -> PathBuf {
    let s = raw.to_string_lossy();
    if s == "~" {
        return crate::paths::home();
    }
    let rest = s.strip_prefix("~/").or_else(|| if cfg!(windows) { s.strip_prefix("~\\") } else { None });
    match rest {
        Some(rest) => crate::paths::home().join(rest),
        None => PathBuf::from(raw),
    }
}

#[derive(Deserialize)]
struct Settings {
    #[serde(rename = "sessionDir")]
    session_dir: Option<Loose>,
}

/// `sessionDir` de los ajustes globales, si es una ruta absoluta (una relativa
/// depende del proyecto desde el que se lance Pi y aquí no se puede saber).
fn settings_session_dir(home: &Path) -> Option<PathBuf> {
    let path = home.join("settings.json");
    if std::fs::metadata(&path).ok()?.len() > MAX_SETTINGS {
        return None;
    }
    let settings: Settings = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    let dir = expand_tilde(OsStr::new(settings.session_dir?.text()?.trim()));
    dir.is_absolute().then_some(dir)
}

/// Raíz de sesiones: variable de entorno > `sessionDir` de los ajustes > `<home>/sessions`.
fn sessions_root(home: &Path, env: Option<OsString>) -> PathBuf {
    match env {
        Some(dir) if !dir.is_empty() => expand_tilde(&dir),
        _ => settings_session_dir(home).unwrap_or_else(|| home.join("sessions")),
    }
}

fn is_jsonl(entry: &std::fs::DirEntry) -> bool {
    // El `file_type` de la entrada no sigue enlaces simbólicos.
    entry.file_type().is_ok_and(|t| t.is_file()) && entry.path().extension().is_some_and(|e| e.eq_ignore_ascii_case("jsonl"))
}

/// Los `.jsonl` de la raíz (layout plano de un `sessionDir` propio) y los de
/// sus subcarpetas directas (`--<cwd>--/`). No sigue enlaces simbólicos.
pub fn units(home: &Path) -> Vec<PathBuf> {
    scan(&sessions_root(home, std::env::var_os("PI_CODING_AGENT_SESSION_DIR")))
}

fn scan(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) => {
            // Sin Pi usado todavía no hay carpeta: no es un fallo.
            if e.kind() != std::io::ErrorKind::NotFound {
                warn_once(root, "no se pudo leer la carpeta de sesiones");
            }
            return out;
        }
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            let Ok(files) = std::fs::read_dir(entry.path()) else { continue };
            out.extend(files.flatten().filter(is_jsonl).map(|f| f.path()));
        } else if is_jsonl(&entry) {
            out.push(entry.path());
        }
    }
    out
}

/// Avisa en el log una sola vez por ruta (el worker reindexa cada ~3 s).
fn warn_once(path: &Path, what: &str) {
    static WARNED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let set = WARNED.get_or_init(Default::default);
    if let Ok(mut set) = set.lock()
        && set.insert(path.to_path_buf())
    {
        crate::warn_log!("pi: {what}: {}", path.display());
    }
}

/// Indexa (o actualiza) una sesión; si falla, conserva lo que había.
///
/// `mtime_ms` es el de modificación del archivo: Pi solo escribe al terminar
/// cada mensaje, así que coincide con la última actividad (salvo en archivos copiados).
pub fn index(_home: &Path, unit: &Path, prev: &Prev) -> Vec<(PathBuf, SessionMeta)> {
    let old = prev.get(unit);
    match sessions::index_jsonl(unit, old, Harness::Pi, on_line) {
        Ok(mut meta) => {
            // Sin `id` en la cabecera, el id es el del nombre `<fecha>_<id>.jsonl`
            // (`index_jsonl` parte de `<fecha>_<id>` y `on_line` no lo cambió).
            let stem = unit.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            if meta.id == stem
                && let Some((_, id)) = stem.split_once('_')
                && !id.is_empty()
            {
                meta.id = id.to_string();
            }
            vec![(unit.to_path_buf(), meta)]
        }
        // Borrada entre el listado y la lectura: ya no existe.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            warn_once(unit, &format!("no se pudo indexar ({})", e.kind()));
            old.map(|m| (unit.to_path_buf(), m.clone())).into_iter().collect()
        }
    }
}

/// Argumentos de `pi`. Reanuda y bifurca por ruta del `.jsonl` (`file`); sin
/// ella, por id. Las sesiones nuevas y los forks reciben `--session-id`, así
/// que su id se conoce de antemano.
pub fn args(mode: &Mode, opts: &LaunchOptions, file: Option<&Path>) -> (Vec<String>, Option<String>) {
    let target = |id: &str| file.map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| id.to_string());
    let mut a = Vec::new();
    let id = match mode {
        Mode::New => {
            let id = uuid::Uuid::new_v4().to_string();
            a.extend(["--session-id".to_string(), id.clone()]);
            if let Some(n) = opts.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                a.extend(["--name".to_string(), n.to_string()]);
            }
            id
        }
        Mode::Resume(id) => {
            a.extend(["--session".to_string(), target(id)]);
            id.clone()
        }
        Mode::Fork(id) => {
            let new = uuid::Uuid::new_v4().to_string();
            a.extend(["--fork".to_string(), target(id), "--session-id".to_string(), new.clone()]);
            new
        }
    };
    if let Some(m) = opts.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        a.extend(["--model".to_string(), m.to_string()]);
    }
    match opts.permission_mode.as_deref() {
        Some(READ_ONLY) => a.extend(["--tools".to_string(), READ_ONLY_TOOLS.to_string()]),
        Some(NO_TOOLS) => a.push("--no-tools".to_string()),
        _ => {}
    }
    (a, Some(id))
}

// ---------------------------------------------------------------- líneas

/// Cadena tolerante: un campo con otro tipo se ignora en vez de invalidar la línea.
#[derive(Deserialize)]
#[serde(untagged)]
enum Loose {
    Text(String),
    Other(IgnoredAny),
}

impl Loose {
    fn text(&self) -> Option<&str> {
        match self {
            Loose::Text(s) => Some(s),
            Loose::Other(_) => None,
        }
    }
}

fn text_of(v: &Option<Loose>) -> Option<&str> {
    v.as_ref().and_then(Loose::text)
}

/// Distingue `"content": null` (`Some(None)`) de que falte (`None`).
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Deserialize)]
struct Entry<'a> {
    #[serde(rename = "type")]
    kind: Option<Loose>,
    id: Option<Loose>,
    cwd: Option<Loose>,
    timestamp: Option<Loose>,
    name: Option<Loose>,
    provider: Option<Loose>,
    #[serde(rename = "modelId")]
    model_id: Option<Loose>,
    #[serde(borrow)]
    message: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Msg<'a> {
    role: Option<Loose>,
    #[serde(default, borrow, deserialize_with = "present")]
    content: Option<Option<&'a RawValue>>,
    provider: Option<Loose>,
    model: Option<Loose>,
    #[serde(rename = "stopReason")]
    stop_reason: Option<Loose>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Block>),
    Other(IgnoredAny),
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: Option<Loose>,
    text: Option<Loose>,
}

/// Texto del mensaje de usuario: la cadena o los bloques `text` unidos por espacio.
fn user_text(raw: &RawValue) -> String {
    match serde_json::from_str::<Content>(raw.get()) {
        Ok(Content::Text(s)) => s,
        Ok(Content::Blocks(blocks)) => {
            let parts: Vec<&str> = blocks.iter().filter(|b| text_of(&b.kind) == Some("text")).filter_map(|b| text_of(&b.text)).collect();
            parts.join(" ")
        }
        _ => String::new(),
    }
}

fn model_name(provider: Option<&str>, model: Option<&str>) -> Option<String> {
    let model = model.map(str::trim).filter(|m| !m.is_empty())?;
    match provider.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => Some(format!("{p}/{model}")),
        None => Some(model.to_string()),
    }
}

/// El analizador solo ve las líneas nuevas, así que el estado vive en `SessionMeta`:
/// - `entrypoint == None` y `!hidden`: aún sin cabecera;
/// - `entrypoint == None` y `hidden`: la primera línea parseable no es una cabecera, no es sesión;
/// - `entrypoint == Some("cli")`: cabecera vista;
/// - `hidden` con cabecera: hubo un mensaje con `content: null`, que Pi no lista.
fn on_line(line: &[u8], meta: &mut SessionMeta) {
    if meta.hidden {
        return;
    }
    let header_seen = meta.entrypoint.is_some();
    if line.len() > MAX_LINE || (!header_seen && line.len() > MAX_HEADER) {
        // Una primera línea así no puede ser la cabecera: Pi tampoco la admite.
        if !header_seen {
            meta.hidden = true;
        } else if oversized_user(line) {
            // Un prompt con imágenes enormes sigue siendo un prompt: se cuenta sin leer su texto.
            meta.prompts += 1;
            meta.has_messages = true;
            meta.busy = true;
        }
        return;
    }
    // Las líneas enormes suelen ser salidas de herramientas: solo interesan mensajes y cambios de modelo o nombre.
    if line.len() > BIG_LINE && header_seen {
        let wanted: [&[u8]; 3] = [b"\"type\":\"message\"", b"\"type\":\"session_info\"", b"\"type\":\"model_change\""];
        if !wanted.iter().any(|needle| memchr::memmem::find(line, needle).is_some()) {
            return;
        }
    }
    let Ok(entry) = serde_json::from_slice::<Entry>(line) else { return };
    let kind = text_of(&entry.kind).unwrap_or("");

    if !header_seen {
        if kind != "session" {
            meta.hidden = true;
            return;
        }
        meta.entrypoint = Some("cli".into());
        if let Some(id) = text_of(&entry.id).filter(|i| !i.is_empty()) {
            meta.id = id.to_string();
        }
        meta.cwd = text_of(&entry.cwd).filter(|c| !c.is_empty()).map(str::to_string);
        meta.started_ms = text_of(&entry.timestamp).and_then(|t| t.parse::<jiff::Timestamp>().ok()).map(|t| t.as_millisecond());
        return;
    }

    match kind {
        // El último nombre gana; uno vacío lo borra.
        "session_info" => meta.custom_title = text_of(&entry.name).map(collapse).filter(|n| !n.is_empty()),
        "model_change" => {
            if let Some(m) = model_name(text_of(&entry.provider), text_of(&entry.model_id)) {
                meta.model = Some(m);
            }
        }
        "message" => {
            if let Some(raw) = entry.message {
                on_message(raw, meta);
            }
        }
        _ => {}
    }
}

/// Una línea demasiado grande para parsearla es un mensaje de usuario si empieza
/// por `{"type":"message",…,"message":{"role":"user"` (Pi escribe las claves en ese orden).
fn oversized_user(line: &[u8]) -> bool {
    const KEY: &[u8] = b"\"message\":{\"role\":\"user\"";
    let head = &line[..line.len().min(512)];
    head.starts_with(b"{\"type\":\"message\"") && memchr::memmem::find(head, KEY).is_some()
}

fn on_message(raw: &RawValue, meta: &mut SessionMeta) {
    let Ok(msg) = serde_json::from_str::<Msg>(raw.get()) else { return };
    let role = text_of(&msg.role).unwrap_or("");
    if matches!(msg.content, Some(None)) && matches!(role, "user" | "assistant") {
        // Pi no lista las sesiones con un mensaje de usuario o asistente de contenido nulo
        // (con otro rol ni lo mira).
        meta.hidden = true;
        meta.has_messages = false;
        return;
    }
    match role {
        "user" => {
            meta.prompts += 1;
            meta.has_messages = true;
            meta.busy = true;
            if let Some(Some(content)) = msg.content {
                let text = user_text(content);
                if meta.first_prompt.is_none() {
                    set_text(&mut meta.first_prompt, Some(text.clone()));
                }
                set_text(&mut meta.last_prompt, Some(text));
            }
        }
        "assistant" => {
            // `stop`, `length`, `error` y `aborted` cierran el turno; `toolUse` sigue.
            meta.busy = text_of(&msg.stop_reason) == Some("toolUse");
            if let Some(m) = model_name(text_of(&msg.provider), text_of(&msg.model)) {
                meta.model = Some(m);
            }
        }
        // Tras el resultado de una herramienta responde el asistente.
        "toolResult" => meta.busy = true,
        // `bashExecution` (un comando `!` del usuario) y demás no cambian el turno.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Carpeta temporal única que se borra al soltarla.
    struct Tmp(PathBuf);

    impl Tmp {
        fn new(name: &str) -> Tmp {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join("claunch-tests").join(format!("pi-{name}-{}-{n}", std::process::id()));
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

    // Fixtures sintéticos con la estructura real de Pi 0.81 (contenido inventado).
    const HEADER: &str = r#"{"type":"session","version":3,"id":"0192a000-aaaa-7000-8000-000000000001","timestamp":"2026-01-02T10:00:00.000Z","cwd":"/home/ana/demo"}"#;
    const USER1: &str = r#"{"type":"message","id":"a1b2c3d4","parentId":null,"timestamp":"2026-01-02T10:00:01.000Z","message":{"role":"user","content":"Hola   ejemplo","timestamp":1767348001000}}"#;
    const ASSIST_TOOL: &str = r#"{"type":"message","id":"b2c3d4e5","parentId":"a1b2c3d4","timestamp":"2026-01-02T10:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"x","thinkingSignature":"sig"},{"type":"toolCall","id":"t1","name":"read","arguments":{}}],"provider":"anthropic","model":"claude-sonnet-5-5","stopReason":"toolUse","timestamp":1767348002000}}"#;
    const TOOL_RESULT: &str = r#"{"type":"message","id":"c3d4e5f6","parentId":"b2c3d4e5","timestamp":"2026-01-02T10:00:03.000Z","message":{"role":"toolResult","toolCallId":"t1","toolName":"read","content":[{"type":"text","text":"salida"}],"isError":false,"timestamp":1767348003000}}"#;
    const ASSIST_STOP: &str = r#"{"type":"message","id":"d4e5f6a7","parentId":"c3d4e5f6","timestamp":"2026-01-02T10:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Listo"}],"provider":"openai","model":"gpt-x","stopReason":"stop","timestamp":1767348004000}}"#;
    const USER2: &str = r#"{"type":"message","id":"e5f6a7b8","parentId":"d4e5f6a7","timestamp":"2026-01-02T10:01:00.000Z","message":{"role":"user","content":[{"type":"text","text":"Segundo"},{"type":"image","data":"AAAA","mimeType":"image/png"},{"type":"text","text":"mensaje"}],"timestamp":1767348060000}}"#;
    const NAME: &str = r#"{"type":"session_info","id":"f6a7b8c9","parentId":"e5f6a7b8","timestamp":"2026-01-02T10:01:30.000Z","name":"Demo de\nnombre"}"#;
    const NAME_EMPTY: &str = r#"{"type":"session_info","id":"07b8c9d0","parentId":"f6a7b8c9","timestamp":"2026-01-02T10:01:40.000Z","name":"  "}"#;
    const MODEL_CHANGE: &str = r#"{"type":"model_change","id":"18c9d0e1","parentId":"07b8c9d0","timestamp":"2026-01-02T10:01:50.000Z","provider":"google","modelId":"gemini-x"}"#;
    const THINKING: &str = r#"{"type":"thinking_level_change","id":"29d0e1f2","parentId":"18c9d0e1","timestamp":"2026-01-02T10:01:55.000Z","thinkingLevel":"high"}"#;
    const BASH: &str = r#"{"type":"message","id":"3ae1f2a3","parentId":"29d0e1f2","timestamp":"2026-01-02T10:02:00.000Z","message":{"role":"bashExecution","command":"ls","output":"x","exitCode":0,"timestamp":1767348120000}}"#;

    fn lines(parts: &[&str]) -> String {
        parts.iter().map(|l| format!("{l}\n")).collect()
    }

    /// Indexa una vez como lo haría el worker.
    fn idx(path: &Path, prev: &Prev) -> Option<SessionMeta> {
        index(Path::new("."), path, prev).into_iter().next().map(|(_, m)| m)
    }

    fn full(path: &Path) -> SessionMeta {
        idx(path, &Prev::new()).unwrap()
    }

    #[test]
    fn info_matches_the_spec() {
        assert_eq!(INFO.install, "npm install -g @earendil-works/pi-coding-agent");
        assert_eq!((INFO.fork, INFO.name, INFO.worktree), (true, true, false));
        assert_eq!(INFO.permissions.first().map(|p| p.value), Some(""));
        let values: Vec<_> = INFO.permissions.iter().map(|p| p.value).collect();
        assert_eq!(values, ["", "read-only", "no-tools"]);
        assert!(matches!(INFO.model, ModelInput::Free(_)));
    }

    #[test]
    fn discovery_reads_flat_and_one_level_deep() {
        let t = Tmp::new("units");
        let a = t.write("--home-ana-demo--/2026-01-02T10-00-00-000Z_aaa.jsonl", HEADER);
        let b = t.write("--home-ana-otro--/2026-01-03T10-00-00-000Z_bbb.JSONL", HEADER);
        let flat = t.write("2026-01-04T10-00-00-000Z_ccc.jsonl", HEADER);
        t.write("--home-ana-demo--/notas.txt", "x");
        t.write("--home-ana-demo--/hondo/2026-01-05T10-00-00-000Z_ddd.jsonl", HEADER);
        std::fs::create_dir_all(t.0.join("vacia")).unwrap();
        let mut got = scan(&t.0);
        got.sort();
        let mut want = vec![a, b, flat];
        want.sort();
        assert_eq!(got, want);
        assert!(scan(&t.0.join("no-existe")).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn discovery_does_not_follow_symlinks() {
        let t = Tmp::new("symlinks");
        let real = t.write("real/2026-01-02T10-00-00-000Z_aaa.jsonl", HEADER);
        let root = t.0.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(t.0.join("real"), root.join("--enlace--")).unwrap();
        std::os::unix::fs::symlink(&real, root.join("x.jsonl")).unwrap();
        assert!(scan(&root).is_empty());
    }

    #[test]
    fn root_priority_env_then_settings_then_default() {
        let t = Tmp::new("root");
        let home = t.0.join("agent");
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(sessions_root(&home, None), home.join("sessions"));
        assert_eq!(sessions_root(&home, Some(OsString::new())), home.join("sessions"));

        let custom = t.0.join("mis sesiones");
        let json = serde_json::json!({ "sessionDir": custom.to_string_lossy(), "otra": 1 }).to_string();
        std::fs::write(home.join("settings.json"), json).unwrap();
        assert_eq!(sessions_root(&home, None), custom);

        let env = t.0.join("env");
        assert_eq!(sessions_root(&home, Some(env.clone().into_os_string())), env);

        // Relativa, de otro tipo o ilegible: se ignora.
        for bad in [r#"{"sessionDir":".pi/sessions"}"#, r#"{"sessionDir":3}"#, "{no es json", r#"{"x":1}"#] {
            std::fs::write(home.join("settings.json"), bad).unwrap();
            assert_eq!(sessions_root(&home, None), home.join("sessions"), "{bad}");
        }
    }

    #[test]
    fn indexes_every_record_variant() {
        let t = Tmp::new("full");
        let p = t.write(
            "--home-ana-demo--/2026-01-02T10-00-00-000Z_0192a000-aaaa-7000-8000-000000000001.jsonl",
            &lines(&[HEADER, USER1, ASSIST_TOOL, TOOL_RESULT, ASSIST_STOP, USER2, NAME, MODEL_CHANGE, THINKING, BASH]),
        );
        let m = full(&p);
        assert_eq!(m.harness, Harness::Pi);
        assert_eq!(m.id, "0192a000-aaaa-7000-8000-000000000001");
        assert_eq!(m.file, p);
        assert_eq!(m.cwd.as_deref(), Some("/home/ana/demo"));
        assert_eq!(m.entrypoint.as_deref(), Some("cli"));
        assert_eq!(m.started_ms, Some(1767348000000));
        assert_eq!(m.prompts, 2);
        assert_eq!(m.first_prompt.as_deref(), Some("Hola ejemplo"));
        assert_eq!(m.last_prompt.as_deref(), Some("Segundo mensaje"));
        assert_eq!(m.custom_title.as_deref(), Some("Demo de nombre"));
        assert_eq!(m.title(), "Demo de nombre");
        // El último `model_change` gana sobre el modelo del asistente anterior.
        assert_eq!(m.model.as_deref(), Some("google/gemini-x"));
        assert!(m.has_messages && !m.hidden);
        // Termina en un comando `!` tras un `user` sin respuesta: el turno sigue abierto.
        assert!(m.busy);
        assert_eq!(m.git_branch, None);
        assert_eq!(m.size, std::fs::metadata(&p).unwrap().len());
        assert!(m.mtime_ms > 0);
    }

    #[test]
    fn model_comes_from_the_last_assistant_message() {
        let t = Tmp::new("model");
        let p = t.write("a/x.jsonl", &lines(&[HEADER, USER1, ASSIST_TOOL, TOOL_RESULT, ASSIST_STOP]));
        assert_eq!(full(&p).model.as_deref(), Some("openai/gpt-x"));
        let q = t.write("a/y.jsonl", &lines(&[HEADER, USER1, MODEL_CHANGE]));
        assert_eq!(full(&q).model.as_deref(), Some("google/gemini-x"));
    }

    #[test]
    fn busy_follows_the_tail_of_the_file() {
        let t = Tmp::new("busy");
        let aborted = ASSIST_STOP.replace("\"stop\"", "\"aborted\"");
        let cases: [(&[&str], bool); 7] = [
            (&[HEADER, USER1], true),
            (&[HEADER, USER1, ASSIST_TOOL], true),
            (&[HEADER, USER1, ASSIST_TOOL, TOOL_RESULT], true),
            (&[HEADER, USER1, ASSIST_TOOL, TOOL_RESULT, ASSIST_STOP], false),
            (&[HEADER, USER1, ASSIST_TOOL, TOOL_RESULT, &aborted], false),
            (&[HEADER, USER1, ASSIST_STOP, BASH], false),
            (&[HEADER, USER1, ASSIST_STOP, USER2], true),
        ];
        for (i, (parts, want)) in cases.iter().enumerate() {
            let p = t.write(&format!("a/{i}.jsonl"), &lines(parts));
            assert_eq!(full(&p).busy, *want, "caso {i}");
        }
    }

    #[test]
    fn name_priority_last_wins_and_empty_clears() {
        let t = Tmp::new("name");
        let first = r#"{"type":"session_info","id":"n1","name":"Primero"}"#;
        let second = r#"{"type":"session_info","id":"n2","name":"Segundo"}"#;
        let p = t.write("a/1.jsonl", &lines(&[HEADER, USER1, first, ASSIST_STOP, second]));
        assert_eq!(full(&p).custom_title.as_deref(), Some("Segundo"));
        // Un nombre vacío posterior lo borra y el título vuelve al primer prompt.
        let q = t.write("a/2.jsonl", &lines(&[HEADER, USER1, first, NAME_EMPTY]));
        let m = full(&q);
        assert_eq!(m.custom_title, None);
        assert_eq!(m.title(), "Hola ejemplo");
        // Un nombre con el tipo equivocado no rompe nada.
        let r = t.write("a/3.jsonl", &lines(&[HEADER, USER1, r#"{"type":"session_info","name":7}"#]));
        assert_eq!(full(&r).custom_title, None);
    }

    #[test]
    fn counts_every_user_message_even_without_text() {
        let t = Tmp::new("prompts");
        let image_only = r#"{"type":"message","id":"i1","message":{"role":"user","content":[{"type":"image","data":"AAAA","mimeType":"image/png"}]}}"#;
        let p = t.write("a/1.jsonl", &lines(&[HEADER, image_only, USER1, ASSIST_STOP, USER2]));
        let m = full(&p);
        assert_eq!(m.prompts, 3);
        // El primer prompt es el primero con texto.
        assert_eq!(m.first_prompt.as_deref(), Some("Hola ejemplo"));
        assert_eq!(m.last_prompt.as_deref(), Some("Segundo mensaje"));
    }

    #[test]
    fn exclusions() {
        let t = Tmp::new("excl");
        // Solo cabecera (fork de una sesión vacía): válida pero sin mensajes.
        let only_header = full(&t.write("a/1.jsonl", &lines(&[r#"{"type":"session","version":3,"id":"h","cwd":"/x","parentSession":"/y.jsonl"}"#])));
        assert!(!only_header.has_messages && !only_header.hidden);
        // Sin mensajes de usuario.
        let no_user = full(&t.write("a/2.jsonl", &lines(&[HEADER, ASSIST_STOP])));
        assert!(!no_user.has_messages);
        // La primera línea parseable no es una cabecera.
        let no_header = full(&t.write("a/3.jsonl", &lines(&[USER1, HEADER, ASSIST_STOP])));
        assert!(no_header.hidden && !no_header.has_messages);
        let wrong = full(&t.write("a/4.jsonl", &lines(&[r#"{"type":"custom","id":"c"}"#, HEADER, USER1])));
        assert!(wrong.hidden && !wrong.has_messages);
        // Pi no lista las sesiones con un `content` nulo.
        let null_content = r#"{"type":"message","id":"z","message":{"role":"assistant","content":null,"stopReason":"stop"}}"#;
        let nulled = full(&t.write("a/5.jsonl", &lines(&[HEADER, USER1, null_content, USER2])));
        assert!(nulled.hidden && !nulled.has_messages);
        // Vacío o a medio escribir: nada que listar todavía.
        let empty = full(&t.write("a/6.jsonl", ""));
        assert!(!empty.has_messages && !empty.hidden);
    }

    #[test]
    fn garbage_before_the_header_is_skipped_like_pi_does() {
        let t = Tmp::new("garbage");
        let p = t.write("a/1.jsonl", &format!("esto no es json\n\n{{\"truncada\n{}", lines(&[HEADER, USER1, "{roto", ASSIST_STOP])));
        let m = full(&p);
        assert!(m.has_messages && !m.hidden);
        assert_eq!(m.id, "0192a000-aaaa-7000-8000-000000000001");
        assert_eq!(m.prompts, 1);
    }

    #[test]
    fn header_without_id_uses_the_file_name_and_old_versions_work() {
        let t = Tmp::new("v1");
        // v1: sin `version`, sin `id` ni `parentId` en las entradas.
        let v1_header = r#"{"type":"session","timestamp":"2025-06-01T08:00:00.000Z","cwd":"C:\\Users\\ana\\demo"}"#;
        let v1_user = r#"{"type":"message","timestamp":"2025-06-01T08:00:01.000Z","message":{"role":"user","content":"Hola v1"}}"#;
        let p = t.write("--C--Users-ana-demo--/2025-06-01T08-00-00-000Z_abc_def-123.jsonl", &lines(&[v1_header, v1_user]));
        let m = full(&p);
        // El id puede contener `_`: se parte por el primero.
        assert_eq!(m.id, "abc_def-123");
        assert_eq!(m.cwd.as_deref(), Some(r"C:\Users\ana\demo"));
        assert_eq!(m.prompts, 1);
        assert!(m.has_messages);
        // Cabecera sin cwd ni timestamp.
        let q = t.write("a/x_1.jsonl", &lines(&[r#"{"type":"session","id":"s1"}"#, v1_user]));
        let m = full(&q);
        assert_eq!((m.id.as_str(), m.cwd, m.started_ms), ("s1", None, None));
    }

    #[test]
    fn tolerates_odd_shapes() {
        let t = Tmp::new("odd");
        let odd = [
            r#"{"type":"message","id":"1","message":"no es un objeto"}"#,
            r#"{"type":"message","id":"2"}"#,
            r#"{"type":"message","id":"3","message":{"role":"user"}}"#,
            r#"{"type":"message","id":"4","message":{"role":"user","content":{"raro":true}}}"#,
            r#"{"type":["x"],"id":5}"#,
            r#"{"type":"message","id":"6","message":{"role":42,"content":"x"}}"#,
            r#"{"type":"model_change","provider":3,"modelId":null}"#,
            "[1,2,3]",
            "null",
        ];
        let mut parts = vec![HEADER];
        parts.extend(odd);
        parts.push(ASSIST_STOP);
        let m = full(&t.write("a/1.jsonl", &lines(&parts)));
        assert!(!m.hidden);
        // Los dos `user` sin texto cuentan como prompts; ningún panic.
        assert_eq!(m.prompts, 2);
        assert_eq!(m.first_prompt, None);
        assert_eq!(m.model.as_deref(), Some("openai/gpt-x"));
    }

    #[test]
    fn huge_and_truncated_lines_do_not_abort_the_file() {
        let t = Tmp::new("huge");
        let big = |n: usize| format!(r#"{{"type":"message","id":"b","message":{{"role":"user","content":"{}"}}}}"#, "a".repeat(n));
        let tool_output = format!(r#"{{"type":"custom","id":"c","data":"{}"}}"#, "b".repeat(200_000));
        let toobig = big(9 << 20);
        let mid = big(100_000);
        let p = t.write("a/1.jsonl", &format!("{}{}\n{}\n{}\n{}\n{{\"type\":\"message\",\"mess", lines(&[HEADER, USER1]), toobig, tool_output, mid, ASSIST_STOP));
        let m = full(&p);
        // La de 9 MiB no se parsea pero cuenta, la de 100 KB se lee (y se recorta), la última sin `\n` no cuenta.
        assert_eq!(m.prompts, 3);
        assert_eq!(m.first_prompt.as_deref(), Some("Hola ejemplo"));
        assert!(m.last_prompt.as_deref().is_some_and(|l| l.chars().count() <= 281 && l.starts_with("aaa")));
        assert!(!m.busy);
        // Una cabecera de más de 1 MiB no es una sesión.
        let huge_header = format!(r#"{{"type":"session","id":"h","cwd":"/x","pad":"{}"}}"#, "p".repeat(2 << 20));
        let q = t.write("a/2.jsonl", &lines(&[&huge_header, USER1]));
        assert!(full(&q).hidden);
    }

    #[test]
    fn null_content_only_hides_user_and_assistant_messages() {
        let t = Tmp::new("nullrole");
        let tool_null = r#"{"type":"message","id":"z","message":{"role":"toolResult","content":null}}"#;
        let m = full(&t.write("a/1.jsonl", &lines(&[HEADER, USER1, tool_null, ASSIST_STOP])));
        assert!(m.has_messages && !m.hidden && !m.busy);
        let user_null = r#"{"type":"message","id":"z","message":{"role":"user","content":null}}"#;
        let m = full(&t.write("a/2.jsonl", &lines(&[HEADER, USER1, user_null])));
        assert!(m.hidden && !m.has_messages);
    }

    #[test]
    fn oversized_user_message_still_counts() {
        let t = Tmp::new("bigprompt");
        let img = format!(r#"{{"type":"message","id":"u","message":{{"role":"user","content":[{{"type":"image","data":"{}"}}]}}}}"#, "A".repeat(9 << 20));
        let tool = format!(r#"{{"type":"message","id":"u","message":{{"role":"toolResult","content":"{}"}}}}"#, "A".repeat(9 << 20));
        let p = t.write("a/1.jsonl", &format!("{}{}
", lines(&[HEADER, USER1, ASSIST_STOP]), img));
        let m = full(&p);
        assert_eq!((m.prompts, m.busy, m.last_prompt.as_deref()), (2, true, Some("Hola ejemplo")));
        let q = t.write("a/2.jsonl", &format!("{}{}
", lines(&[HEADER, USER1, ASSIST_STOP]), tool));
        assert_eq!((full(&q).prompts, full(&q).busy), (1, false));
    }

    #[test]
    fn second_pass_is_identical_and_chunked_growth_matches() {
        let t = Tmp::new("determ");
        let p = t.write("a/2026-01-02T10-00-00-000Z_det.jsonl", &lines(&[r#"{"type":"session","cwd":"\\\\srv\\sh\\"}"#, USER1, ASSIST_STOP]));
        let a = full(&p);
        let mut prev = Prev::new();
        prev.insert(p.clone(), a.clone());
        assert_eq!(idx(&p, &prev).unwrap(), a);
        assert_eq!(a.id, "det");
        assert_eq!(a.cwd.as_deref(), Some(r"\\srv\sh\"));
        // Un archivo reescrito con otro contenido de la misma cabecera no deja estado viejo.
        std::fs::write(&p, lines(&[HEADER, USER1])).unwrap();
        assert_eq!(idx(&p, &prev).unwrap(), full(&p));
    }

    #[test]
    fn incremental_equals_full_at_every_cut() {
        let t = Tmp::new("incr");
        let all = lines(&[HEADER, USER1, ASSIST_TOOL, TOOL_RESULT, ASSIST_STOP, USER2, NAME, NAME_EMPTY, MODEL_CHANGE, THINKING, BASH]);
        let p = t.0.join("a").join("2026-01-02T10-00-00-000Z_inc.jsonl");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let reference = t.write("ref/2026-01-02T10-00-00-000Z_inc.jsonl", &all);
        let bytes = all.as_bytes();
        // Cortes en medio de una línea incluidos: la mitad escrita no cuenta hasta que se complete.
        let mut cuts: Vec<usize> = (0..bytes.len()).step_by(37).collect();
        cuts.push(bytes.len());
        std::fs::write(&p, b"").unwrap();
        let mut prev = Prev::new();
        for cut in cuts {
            let mut f = std::fs::OpenOptions::new().write(true).truncate(true).open(&p).unwrap();
            f.write_all(&bytes[..cut]).unwrap();
            drop(f);
            let inc = idx(&p, &prev).unwrap();
            let fresh = full(&p);
            assert_eq!(inc, fresh, "corte {cut}");
            prev.insert(p.clone(), inc);
        }
        // Mismo contenido final que el archivo escrito de una vez.
        let last = prev.get(&p).unwrap();
        let whole = full(&reference);
        assert_eq!((last.prompts, &last.custom_title, &last.model, last.busy), (whole.prompts, &whole.custom_title, &whole.model, whole.busy));
        assert_eq!((&last.first_prompt, &last.last_prompt, &last.cwd), (&whole.first_prompt, &whole.last_prompt, &whole.cwd));
    }

    #[test]
    fn appended_lines_are_read_incrementally() {
        let t = Tmp::new("append");
        let p = t.write("a/2026-01-02T10-00-00-000Z_app.jsonl", &lines(&[HEADER, USER1]));
        let m1 = idx(&p, &Prev::new()).unwrap();
        assert!(m1.busy && m1.prompts == 1);
        let mut prev = Prev::new();
        prev.insert(p.clone(), m1.clone());
        // Sin cambios: devuelve lo mismo.
        assert_eq!(idx(&p, &prev).unwrap(), m1);
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(lines(&[ASSIST_STOP, USER2, NAME]).as_bytes()).unwrap();
        drop(f);
        let m2 = idx(&p, &prev).unwrap();
        assert_eq!(m2.prompts, 2);
        assert_eq!(m2.custom_title.as_deref(), Some("Demo de nombre"));
        assert_eq!(m2.first_prompt.as_deref(), Some("Hola ejemplo"));
        assert_eq!(m2, full(&p));
        // Si el archivo se reescribe (otra cabecera), se relee entero.
        let other = HEADER.replace("/home/ana/demo", "/home/ana/otro-proyecto");
        std::fs::write(&p, lines(&[&other, USER1, ASSIST_STOP, USER2, NAME, THINKING])).unwrap();
        prev.insert(p.clone(), m2);
        let m3 = idx(&p, &prev).unwrap();
        assert_eq!(m3.cwd.as_deref(), Some("/home/ana/otro-proyecto"));
        assert_eq!(m3, full(&p));
    }

    #[test]
    fn unreadable_unit_keeps_the_previous_session() {
        let t = Tmp::new("keep");
        // Un directorio con nombre de sesión: existe pero no se puede leer como archivo.
        let dir = t.0.join("a").join("x.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        let old = SessionMeta { harness: Harness::Pi, id: "viejo".into(), file: dir.clone(), has_messages: true, ..Default::default() };
        let mut prev = Prev::new();
        prev.insert(dir.clone(), old.clone());
        assert_eq!(idx(&dir, &prev), Some(old));
        assert_eq!(idx(&dir, &Prev::new()), None);
        // Borrada: desaparece.
        assert_eq!(idx(&t.0.join("a").join("no-esta.jsonl"), &prev), None);
    }

    fn opts(model: Option<&str>, perm: Option<&str>, name: Option<&str>) -> LaunchOptions {
        LaunchOptions {
            model: model.map(String::from),
            permission_mode: perm.map(String::from),
            name: name.map(String::from),
            ..Default::default()
        }
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn new_session_args() {
        let (a, id) = args(&Mode::New, &opts(None, None, None), None);
        let id = id.unwrap();
        assert!(uuid::Uuid::parse_str(&id).is_ok());
        assert_eq!(a, s(&["--session-id", &id]));

        let (a, id2) = args(&Mode::New, &opts(Some("anthropic/claude-sonnet-5-5"), Some("read-only"), Some("  Mi prueba ")), None);
        let id2 = id2.unwrap();
        assert_ne!(id, id2);
        assert_eq!(a, s(&["--session-id", &id2, "--name", "Mi prueba", "--model", "anthropic/claude-sonnet-5-5", "--tools", "read,grep,find,ls"]));

        let (a, _) = args(&Mode::New, &opts(Some(""), Some("no-tools"), Some("   ")), None);
        assert_eq!(a[2..], s(&["--no-tools"]));
        // Valores de permisos desconocidos (p. ej. de otro harness) se ignoran.
        let (a, _) = args(&Mode::New, &opts(None, Some("bypassPermissions"), None), None);
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn resume_and_fork_args() {
        let file = Path::new("/home/ana/.pi/agent/sessions/--home-ana-demo--/2026_abc.jsonl");
        let path = file.to_string_lossy().into_owned();
        let (a, id) = args(&Mode::Resume("abc".into()), &opts(Some("openai/gpt-x"), Some("no-tools"), Some("ignorado")), Some(file));
        assert_eq!(id.as_deref(), Some("abc"));
        assert_eq!(a, s(&["--session", &path, "--model", "openai/gpt-x", "--no-tools"]));
        // Sin archivo, por id.
        let (a, id) = args(&Mode::Resume("abc".into()), &opts(None, None, None), None);
        assert_eq!((a, id.as_deref()), (s(&["--session", "abc"]), Some("abc")));

        let (a, id) = args(&Mode::Fork("abc".into()), &opts(Some("openai/gpt-x"), Some("read-only"), None), Some(file));
        let new = id.unwrap();
        assert_ne!(new, "abc");
        assert!(uuid::Uuid::parse_str(&new).is_ok());
        assert_eq!(a, s(&["--fork", &path, "--session-id", &new, "--model", "openai/gpt-x", "--tools", "read,grep,find,ls"]));
        let (a, _) = args(&Mode::Fork("abc".into()), &opts(None, None, None), None);
        assert_eq!(a[..2], s(&["--fork", "abc"]));
    }
}
