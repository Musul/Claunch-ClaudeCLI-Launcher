//! Indexador de sesiones de Claude Code (`~/.claude/projects/*/<uuid>.jsonl`).
//!
//! Los `.jsonl` crecen solo por el final, así que el índice es incremental:
//! guarda hasta qué byte leyó y una huella de los últimos bytes. Si la huella
//! coincide, solo parsea lo añadido; si no (archivo reescrito), relee todo.

use std::borrow::Cow;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

const FINGERPRINT_LEN: u64 = 128;
const MAX_TEXT: usize = 280;
/// Mensajes entre sesiones que se guardan por sesión (los más recientes).
pub const MAX_LINKS: usize = 100;

/// Un mensaje entre sesiones visto en el historial de esta sesión.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerLink {
    /// `true`: esta sesión lo envió (`SendMessage`); `false`: lo recibió.
    pub out: bool,
    /// Otro extremo: `uds:<tubería>`, `bridge:session_…` o un nombre de sesión.
    pub peer: String,
    /// Nombre visible del otro extremo (en los recibidos).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `msg_id` del recibido (evita duplicados).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SessionMeta {
    pub id: String,
    pub file: PathBuf,
    pub size: u64,
    pub mtime_ms: i64,
    /// Byte siguiente a la última línea completa leída.
    pub indexed_to: u64,
    /// FNV-1a de los bytes `[indexed_to - 128, indexed_to)`.
    pub fingerprint: u64,
    pub cwd: Option<String>,
    pub custom_title: Option<String>,
    pub ai_title: Option<String>,
    pub agent_name: Option<String>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub git_branch: Option<String>,
    pub entrypoint: Option<String>,
    pub started_ms: Option<i64>,
    pub pr_number: Option<u64>,
    /// Prompts humanos (sin tool results ni mensajes de sistema).
    pub prompts: u32,
    pub has_messages: bool,
    /// Mensajes con otras sesiones (máximo `MAX_LINKS`, del más viejo al más nuevo).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<PeerLink>,
}

impl SessionMeta {
    /// Título visible: nombre puesto por el usuario > título IA > primer prompt.
    pub fn title(&self) -> &str {
        self.custom_title
            .as_deref()
            .or(self.agent_name.as_deref())
            .or(self.ai_title.as_deref())
            .or(self.first_prompt.as_deref())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or("(sin título)")
    }
}

#[derive(Deserialize)]
struct Line<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
    #[serde(borrow)]
    cwd: Option<Cow<'a, str>>,
    #[serde(rename = "gitBranch", borrow)]
    git_branch: Option<Cow<'a, str>>,
    #[serde(borrow)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(borrow)]
    entrypoint: Option<Cow<'a, str>>,
    #[serde(rename = "isMeta")]
    is_meta: Option<bool>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    #[serde(rename = "isCompactSummary")]
    is_compact_summary: Option<bool>,
    #[serde(rename = "toolUseResult")]
    tool_use_result: Option<IgnoredAny>,
    #[serde(borrow)]
    message: Option<&'a RawValue>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "agentName")]
    agent_name: Option<String>,
    #[serde(rename = "lastPrompt")]
    last_prompt: Option<String>,
    #[serde(rename = "prNumber")]
    pr_number: Option<u64>,
    origin: Option<Origin>,
}

/// `origin` de un mensaje recibido de otra sesión (`kind = "peer"`).
#[derive(Deserialize)]
struct Origin {
    kind: Option<String>,
    from: Option<String>,
    name: Option<String>,
    msg_id: Option<String>,
}

#[derive(Deserialize)]
struct ToolMessage<'a> {
    #[serde(borrow)]
    content: Option<Vec<ToolBlock<'a>>>,
}

#[derive(Deserialize)]
struct ToolBlock<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(borrow)]
    name: Option<Cow<'a, str>>,
    input: Option<SendInput>,
}

/// Entrada de la herramienta `SendMessage` (solo los campos de destino).
#[derive(Deserialize)]
struct SendInput {
    to: Option<String>,
    recipient: Option<String>,
    recipient_kind: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    content: Option<Content>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

/// Tipos de línea `{"type":"..."}` que interesan; el resto se salta sin parsear.
const WANTED: [&str; 7] = ["custom-title", "ai-title", "agent-name", "last-prompt", "pr-link", "user", "assistant"];

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Indexa (o actualiza) una sesión. `prev` es la entrada del índice anterior.
pub fn index_file(path: &Path, prev: Option<&SessionMeta>) -> std::io::Result<SessionMeta> {
    let md = std::fs::metadata(path)?;
    let size = md.len();
    let mtime = mtime_ms(&md);
    if let Some(p) = prev
        && p.size == size && p.mtime_ms == mtime && p.file == path {
            return Ok(p.clone());
        }

    let mut file = File::open(path)?;
    let id = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();

    // Incremental: solo si el archivo creció y el final ya indexado no cambió.
    if let Some(p) = prev.filter(|p| p.indexed_to > 0 && size >= p.indexed_to && p.file == path) {
        let fp_start = p.indexed_to.saturating_sub(FINGERPRINT_LEN);
        file.seek(SeekFrom::Start(fp_start))?;
        let mut buf = Vec::with_capacity((size - fp_start) as usize);
        file.read_to_end(&mut buf)?;
        let fp_len = (p.indexed_to - fp_start) as usize;
        if buf.len() >= fp_len && fnv1a(&buf[..fp_len]) == p.fingerprint {
            let mut meta = p.clone();
            meta.size = size;
            meta.mtime_ms = mtime;
            let consumed = parse_lines(&buf[fp_len..], &mut meta);
            advance(&mut meta, &buf, fp_len + consumed, fp_start);
            return Ok(meta);
        }
        file.seek(SeekFrom::Start(0))?;
    }

    let mut buf = Vec::with_capacity(size as usize);
    file.read_to_end(&mut buf)?;
    let mut meta = SessionMeta { id, file: path.to_path_buf(), size, mtime_ms: mtime, ..Default::default() };
    let consumed = parse_lines(&buf, &mut meta);
    advance(&mut meta, &buf, consumed, 0);
    Ok(meta)
}

/// Fija `indexed_to` y la huella; `end` es relativo a `buf`, que empieza en `base`.
fn advance(meta: &mut SessionMeta, buf: &[u8], end: usize, base: u64) {
    meta.indexed_to = base + end as u64;
    // El buffer siempre contiene los `k` bytes previos: en modo incremental
    // empieza `min(128, indexed_to_anterior)` bytes antes de lo nuevo.
    let k = meta.indexed_to.min(FINGERPRINT_LEN) as usize;
    meta.fingerprint = fnv1a(&buf[end.saturating_sub(k)..end]);
}

/// Parsea líneas completas de `buf`; devuelve los bytes consumidos.
fn parse_lines(buf: &[u8], meta: &mut SessionMeta) -> usize {
    let mut pos = 0;
    while let Some(nl) = memchr::memchr(b'\n', &buf[pos..]) {
        let line = &buf[pos..pos + nl];
        pos += nl + 1;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.len() < 2 {
            continue;
        }
        if let Some(rest) = line.strip_prefix(b"{\"type\":\"") {
            let end = memchr::memchr(b'"', rest).unwrap_or(0);
            let kind = &rest[..end];
            if !WANTED.iter().any(|w| w.as_bytes() == kind) {
                continue;
            }
        }
        let Ok(l) = serde_json::from_slice::<Line>(line) else { continue };
        let sends = memchr::memmem::find(line, b"\"SendMessage\"").is_some();
        apply(l, meta, sends);
    }
    pos
}

fn apply(l: Line, meta: &mut SessionMeta, sends: bool) {
    let kind = l.kind.as_deref().unwrap_or("");
    match kind {
        "custom-title" => set_text(&mut meta.custom_title, l.custom_title),
        "ai-title" => set_text(&mut meta.ai_title, l.ai_title),
        "agent-name" => set_text(&mut meta.agent_name, l.agent_name),
        "last-prompt" => {
            if let Some(p) = l.last_prompt.as_deref().and_then(clean_prompt) {
                meta.last_prompt = Some(p);
            }
        }
        "pr-link"
            if l.pr_number.is_some() => {
                meta.pr_number = l.pr_number;
            }
        _ => {}
    }
    if l.is_sidechain == Some(true) {
        return;
    }
    let is_message = kind == "user" || kind == "assistant";
    if let Some(cwd) = l.cwd.as_deref().filter(|c| !c.is_empty())
        && meta.cwd.as_deref() != Some(cwd) {
            meta.cwd = Some(cwd.to_string());
        }
    if let Some(b) = l.git_branch.as_deref().filter(|b| !b.is_empty() && *b != "HEAD")
        && meta.git_branch.as_deref() != Some(b) {
            meta.git_branch = Some(b.to_string());
        }
    if !is_message {
        return;
    }
    meta.has_messages = true;
    let ts_ms = || l.timestamp.as_deref().and_then(|t| t.parse::<jiff::Timestamp>().ok()).map(|t| t.as_millisecond());
    if let Some(o) = l.origin.as_ref().filter(|o| o.kind.as_deref() == Some("peer"))
        && let (Some(from), Some(ts)) = (o.from.as_deref().filter(|f| !f.is_empty()), ts_ms())
        && o.msg_id.as_ref().is_none_or(|id| !meta.links.iter().any(|x| x.id.as_ref() == Some(id)))
    {
        push_link(meta, PeerLink { out: false, peer: from.to_string(), name: o.name.clone(), id: o.msg_id.clone(), ts_ms: ts });
    }
    if kind == "assistant"
        && sends
        && let Some(ts) = ts_ms()
        && let Some(raw) = l.message
        && let Ok(msg) = serde_json::from_str::<ToolMessage>(raw.get())
    {
        for b in msg.content.unwrap_or_default() {
            if b.kind != "tool_use" || b.name.as_deref() != Some("SendMessage") {
                continue;
            }
            let Some(input) = b.input else { continue };
            if input.recipient_kind.as_deref() == Some("agent") {
                continue;
            }
            let Some(to) = input.to.or(input.recipient).map(|t| t.trim().to_string()) else { continue };
            if to.is_empty() || to == "*" || is_agent_id(&to) {
                continue;
            }
            push_link(meta, PeerLink { out: true, peer: to, name: None, id: None, ts_ms: ts });
        }
    }
    if meta.entrypoint.is_none() {
        meta.entrypoint = l.entrypoint.map(Cow::into_owned);
    }
    if meta.started_ms.is_none() {
        meta.started_ms = l
            .timestamp
            .as_deref()
            .and_then(|t| t.parse::<jiff::Timestamp>().ok())
            .map(|t| t.as_millisecond());
    }
    if kind != "user" || l.tool_use_result.is_some() || l.is_meta == Some(true) || l.is_compact_summary == Some(true) {
        return;
    }
    let Some(raw) = l.message else { return };
    let Ok(msg) = serde_json::from_str::<Message>(raw.get()) else { return };
    let text = match msg.content {
        Some(Content::Text(t)) => t,
        Some(Content::Blocks(blocks)) => {
            if blocks.iter().any(|b| b.kind == "tool_result") {
                return;
            }
            let mut out = String::new();
            for b in blocks {
                match (b.kind.as_str(), b.text) {
                    ("text", Some(t)) => {
                        if !out.is_empty() {
                            out.push(' ');
                        }
                        out.push_str(&t);
                    }
                    ("image", _) => out.push_str(" [imagen]"),
                    _ => {}
                }
            }
            out
        }
        None => return,
    };
    if let Some(p) = clean_prompt(&text) {
        meta.prompts += 1;
        if meta.first_prompt.is_none() {
            meta.first_prompt = Some(p);
        }
    }
}

fn push_link(meta: &mut SessionMeta, link: PeerLink) {
    meta.links.push(link);
    if meta.links.len() > MAX_LINKS {
        let extra = meta.links.len() - MAX_LINKS;
        meta.links.drain(..extra);
    }
}

/// Id de subagente (`a` + 16 hex): mensajes internos de la sesión, no entre sesiones.
fn is_agent_id(s: &str) -> bool {
    s.len() == 17 && s.starts_with('a') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn set_text(slot: &mut Option<String>, value: Option<String>) {
    if let Some(v) = value.map(|v| collapse(&v)).filter(|v| !v.is_empty()) {
        *slot = Some(v);
    }
}

fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].trim())
}

/// Texto legible de un prompt, o `None` si es contenido generado por el sistema.
pub fn clean_prompt(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(name) = tag(t, "command-name") {
        let args = tag(t, "command-args").unwrap_or("");
        let cmd = if name.starts_with('/') { name.to_string() } else { format!("/{name}") };
        return Some(collapse(&format!("{cmd} {args}")));
    }
    if let Some(cmd) = tag(t, "bash-input") {
        return Some(collapse(&format!("! {cmd}")));
    }
    if t.starts_with('<') || t.starts_with("Caveat:") || t.starts_with("[Request interrupted") {
        return None;
    }
    Some(collapse(t))
}

/// Colapsa espacios y recorta a `MAX_TEXT` caracteres.
fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_TEXT + 4));
    let mut count = 0;
    for word in s.split_whitespace() {
        if count > 0 {
            out.push(' ');
            count += 1;
        }
        for ch in word.chars() {
            if count >= MAX_TEXT {
                out.push('…');
                return out;
            }
            out.push(ch);
            count += 1;
        }
    }
    out
}

/// Lista los `.jsonl` de primer nivel de cada carpeta de proyecto.
pub fn list_session_files(projects: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(projects) else {
        crate::warn_log!("no se pudo leer {}", projects.display());
        return out;
    };
    for dir in dirs.flatten() {
        if !dir.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(dir.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            let is_jsonl = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("jsonl"));
            if is_jsonl && f.file_type().map(|t| t.is_file()).unwrap_or(false) {
                out.push(p);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_tmp(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("claunch-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    const USER: &str = r#"{"parentUuid":null,"isSidechain":false,"type":"user","timestamp":"2026-11-02T14:44:11.943Z","entrypoint":"claude-vscode","cwd":"c:\\Users\\X\\repo","gitBranch":"main","message":{"role":"user","content":"hola   mundo"}}"#;
    const TOOL: &str = r#"{"parentUuid":"a","isSidechain":false,"type":"user","cwd":"c:\\Users\\X\\repo","toolUseResult":{"x":1},"message":{"role":"user","content":[{"type":"tool_result","content":"x"}]}}"#;
    const CMD: &str = r#"{"parentUuid":"b","type":"user","cwd":"c:\\Users\\X\\repo","message":{"role":"user","content":"<command-name>/model</command-name>\n<command-args>opus</command-args>"}}"#;
    const AI: &str = r#"{"type":"ai-title","aiTitle":"Título IA","sessionId":"s"}"#;
    const CUSTOM: &str = r#"{"type":"custom-title","customTitle":"Mi nombre","sessionId":"s"}"#;
    const BLOB: &str = r#"{"type":"api-request-blob","data":"{\"type\":\"user\"}"}"#;

    #[test]
    fn parses_metadata_and_prompts() {
        let content = format!("{USER}\n{TOOL}\n{BLOB}\n{AI}\n{CMD}\n");
        let p = write_tmp("a1.jsonl", &content);
        let m = index_file(&p, None).unwrap();
        assert_eq!(m.cwd.as_deref(), Some(r"c:\Users\X\repo"));
        assert_eq!(m.first_prompt.as_deref(), Some("hola mundo"));
        assert_eq!(m.prompts, 2);
        assert_eq!(m.ai_title.as_deref(), Some("Título IA"));
        assert_eq!(m.title(), "Título IA");
        assert_eq!(m.entrypoint.as_deref(), Some("claude-vscode"));
        assert_eq!(m.git_branch.as_deref(), Some("main"));
        assert!(m.started_ms.is_some());
        assert_eq!(m.indexed_to, content.len() as u64);
    }

    #[test]
    fn incremental_append_matches_full() {
        let first = format!("{USER}\n{AI}\n");
        let p = write_tmp("a2.jsonl", &first);
        let m1 = index_file(&p, None).unwrap();
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        write!(f, "{CUSTOM}\n{CMD}\n").unwrap();
        drop(f);
        // Fuerza mtime distinto aunque el FS tenga poca resolución.
        let mut stale = m1.clone();
        stale.mtime_ms -= 1;
        let inc = index_file(&p, Some(&stale)).unwrap();
        let full = index_file(&p, None).unwrap();
        assert_eq!(inc.title(), "Mi nombre");
        assert_eq!(inc.prompts, full.prompts);
        assert_eq!(inc.indexed_to, full.indexed_to);
        assert_eq!(inc.fingerprint, full.fingerprint);
        assert_eq!(inc.first_prompt, full.first_prompt);
    }

    #[test]
    fn rewritten_file_triggers_full_reindex() {
        let p = write_tmp("a3.jsonl", &format!("{USER}\n{AI}\n"));
        let m1 = index_file(&p, None).unwrap();
        std::fs::write(&p, format!("{CUSTOM}\n{USER}\n{USER}\n")).unwrap();
        let mut stale = m1.clone();
        stale.mtime_ms -= 1;
        let m2 = index_file(&p, Some(&stale)).unwrap();
        assert_eq!(m2.ai_title, None);
        assert_eq!(m2.title(), "Mi nombre");
        assert_eq!(m2.prompts, 2);
    }

    #[test]
    fn partial_last_line_is_left_for_later() {
        let content = format!("{USER}\n{{\"type\":\"ai-title\",\"aiTi");
        let p = write_tmp("a4.jsonl", &content);
        let m = index_file(&p, None).unwrap();
        assert_eq!(m.indexed_to, USER.len() as u64 + 1);
        assert_eq!(m.ai_title, None);
    }

    const PEER_IN: &str = r#"{"type":"user","isMeta":true,"timestamp":"2026-11-04T15:30:31.739Z","cwd":"c:/Users/X/repo","message":{"role":"user","content":"x"},"origin":{"kind":"peer","from":"uds:cc-msg-1","name":"Otra","msg_id":"m1","body":"hola"}}"#;
    const PEER_OUT: &str = r#"{"type":"assistant","timestamp":"2026-11-04T15:31:00.000Z","cwd":"c:/Users/X/repo","message":{"role":"assistant","content":[{"type":"text","text":"ok"},{"type":"tool_use","id":"t1","name":"SendMessage","input":{"to":"bridge:session_01ABC","message":"hola","recipient_kind":"session"}},{"type":"tool_use","id":"t2","name":"SendMessage","input":{"to":"a9c576fceb902380d","message":"x"}}]}}"#;

    #[test]
    fn captures_peer_links() {
        let content = format!("{USER}\n{PEER_IN}\n{PEER_IN}\n{PEER_OUT}\n");
        let p = write_tmp("a5.jsonl", &content);
        let m = index_file(&p, None).unwrap();
        assert_eq!(m.links.len(), 2, "{:?}", m.links);
        assert!(!m.links[0].out);
        assert_eq!(m.links[0].peer, "uds:cc-msg-1");
        assert_eq!(m.links[0].name.as_deref(), Some("Otra"));
        assert!(m.links[1].out);
        assert_eq!(m.links[1].peer, "bridge:session_01ABC");
        // Los mensajes de sistema no cuentan como prompts.
        assert_eq!(m.prompts, 1);
    }

    #[test]
    fn clean_prompt_rules() {
        assert_eq!(clean_prompt("<local-command-stdout>x</local-command-stdout>"), None);
        assert_eq!(clean_prompt("<bash-input>ls -la</bash-input>").as_deref(), Some("! ls -la"));
        assert_eq!(clean_prompt("  a\n\nb ").as_deref(), Some("a b"));
    }
}
