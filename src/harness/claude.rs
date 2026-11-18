//! Claude Code: sesiones en `~/.claude/projects/*/<uuid>.jsonl`.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::value::RawValue;

use super::{Harness, Info, ModelInput, Perm, Prev};
use crate::config::LaunchOptions;
use crate::data::sessions::{self, PeerLink, SessionMeta, clean_prompt, set_text};
use crate::launch::Mode;

pub const INFO: Info = Info {
    id: "claude",
    label: "Claude Code",
    tag: "claude",
    program: "claude",
    install: if cfg!(windows) { "irm https://claude.ai/install.ps1 | iex" } else { "curl -fsSL https://claude.ai/install.sh | bash" },
    color: "#D97757",
    fork: true,
    name: true,
    worktree: true,
    model: ModelInput::Choice(&["", "fable", "opus", "sonnet", "haiku"]),
    permissions: &[
        Perm { value: "", label: "por defecto", hint: "" },
        Perm { value: "manual", label: "manual", hint: "pregunta antes de actuar" },
        Perm { value: "acceptEdits", label: "acceptEdits", hint: "acepta ediciones sin preguntar" },
        Perm { value: "auto", label: "auto", hint: "" },
        Perm { value: "plan", label: "plan", hint: "solo planifica, no edita" },
        Perm { value: "dontAsk", label: "dontAsk", hint: "" },
        Perm { value: "bypassPermissions", label: "bypassPermissions", hint: "sin preguntas de permisos" },
    ],
};

/// Carpeta de datos (`CLAUDE_CONFIG_DIR` o `~/.claude`).
pub fn home() -> PathBuf {
    crate::paths::claude_home()
}

/// Los `.jsonl` de primer nivel de cada carpeta de proyecto.
pub fn units(home: &Path) -> Vec<PathBuf> {
    let projects = home.join("projects");
    let mut out = Vec::new();
    let Ok(dirs) = std::fs::read_dir(&projects) else {
        // Sin Claude instalado no hay carpeta: no es un fallo.
        if home.is_dir() {
            crate::warn_log!("no se pudo leer {}", projects.display());
        }
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

/// Indexa (o actualiza) una sesión; si falla, conserva lo que había.
pub fn index(unit: &Path, prev: &Prev) -> Vec<(PathBuf, SessionMeta)> {
    let old = prev.get(unit);
    let meta = match sessions::index_jsonl(unit, old, Harness::Claude, on_line) {
        Ok(m) => Some(m),
        Err(e) => {
            crate::warn_log!("no se pudo indexar {}: {e}", unit.display());
            old.cloned()
        }
    };
    meta.map(|m| (unit.to_path_buf(), m)).into_iter().collect()
}

/// Argumentos de `claude`. Las sesiones nuevas reciben `--session-id`, así que
/// su id se conoce de antemano.
pub fn args(mode: &Mode, opts: &LaunchOptions) -> (Vec<String>, Option<String>) {
    let mut a = Vec::new();
    let id = match mode {
        Mode::New => {
            let id = uuid::Uuid::new_v4().to_string();
            a.extend(["--session-id".to_string(), id.clone()]);
            if let Some(n) = opts.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                a.extend(["-n".to_string(), n.to_string()]);
            }
            if let Some(w) = opts.worktree.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
                a.extend(["-w".to_string(), w.to_string()]);
            }
            Some(id)
        }
        Mode::Resume(id) => {
            a.extend(["-r".to_string(), id.clone()]);
            Some(id.clone())
        }
        Mode::Fork(id) => {
            a.extend(["-r".to_string(), id.clone(), "--fork-session".to_string()]);
            None
        }
    };
    if let Some(m) = opts.model.as_deref().filter(|m| !m.is_empty()) {
        a.extend(["--model".to_string(), m.to_string()]);
    }
    if let Some(p) = opts.permission_mode.as_deref().filter(|p| !p.is_empty()) {
        a.extend(["--permission-mode".to_string(), p.to_string()]);
    }
    (a, id)
}

// ---------------------------------------------------------------- líneas

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

fn on_line(line: &[u8], meta: &mut SessionMeta) {
    if let Some(rest) = line.strip_prefix(b"{\"type\":\"") {
        let end = memchr::memchr(b'"', rest).unwrap_or(0);
        let kind = &rest[..end];
        if !WANTED.iter().any(|w| w.as_bytes() == kind) {
            return;
        }
    }
    let Ok(l) = serde_json::from_slice::<Line>(line) else { return };
    let sends = memchr::memmem::find(line, b"\"SendMessage\"").is_some();
    apply(l, meta, sends);
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
        sessions::push_link(meta, PeerLink { out: false, peer: from.to_string(), name: o.name.clone(), id: o.msg_id.clone(), ts_ms: ts });
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
            sessions::push_link(meta, PeerLink { out: true, peer: to, name: None, id: None, ts_ms: ts });
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

/// Id de subagente (`a` + 16 hex): mensajes internos de la sesión, no entre sesiones.
fn is_agent_id(s: &str) -> bool {
    s.len() == 17 && s.starts_with('a') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
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

    fn index_file(path: &Path, prev: Option<&SessionMeta>) -> std::io::Result<SessionMeta> {
        sessions::index_jsonl(path, prev, Harness::Claude, on_line)
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
        assert_eq!(m.harness, Harness::Claude);
        assert_eq!(m.id, "a1");
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
    fn args_for_modes() {
        let opts = LaunchOptions { model: Some("opus".into()), name: Some("x".into()), ..Default::default() };
        let (a, id) = args(&Mode::New, &opts);
        assert_eq!(a[0], "--session-id");
        assert_eq!(id.as_deref(), Some(a[1].as_str()));
        assert!(a.windows(2).any(|w| w == ["-n", "x"]));
        assert!(a.windows(2).any(|w| w == ["--model", "opus"]));
        let (a, id) = args(&Mode::Fork("abc".into()), &LaunchOptions::default());
        assert_eq!(a, vec!["-r", "abc", "--fork-session"]);
        assert_eq!(id, None);
    }

    #[test]
    fn units_lists_first_level_jsonl() {
        let home = std::env::temp_dir().join("claunch-tests").join("claude-home");
        let proj = home.join("projects").join("c--r-app");
        std::fs::create_dir_all(proj.join("0f6c2b1e").join("subagents")).unwrap();
        std::fs::write(proj.join("0f6c2b1e.jsonl"), "").unwrap();
        std::fs::write(proj.join("0f6c2b1e").join("subagents").join("agent-1.jsonl"), "").unwrap();
        std::fs::write(proj.join("notas.txt"), "").unwrap();
        let found = units(&home);
        assert_eq!(found, vec![proj.join("0f6c2b1e.jsonl")]);
        assert!(units(&home.join("no-existe")).is_empty());
    }
}
