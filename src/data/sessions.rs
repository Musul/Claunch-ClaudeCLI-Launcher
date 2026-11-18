//! Metadatos de una sesión e índice incremental de archivos `.jsonl`.
//!
//! Los `.jsonl` de sesión (Claude Code, Codex, Pi) crecen solo por el final,
//! así que el índice es incremental: guarda hasta qué byte leyó y una huella de
//! los últimos bytes. Si la huella coincide, solo parsea lo añadido; si no
//! (archivo reescrito), relee todo. Cada harness aporta su analizador de líneas
//! (ver `crate::harness`).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::harness::Harness;

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
    /// Harness al que pertenece la sesión.
    #[serde(default)]
    pub harness: Harness,
    pub id: String,
    /// Archivo de la sesión o, si el harness usa una base de datos, la base.
    pub file: PathBuf,
    /// Tamaño del archivo de la sesión (0 si no tiene archivo propio).
    pub size: u64,
    /// Última actividad.
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
    /// Superficie desde la que se creó: `cli`, `vscode`, `desktop`… (Claude
    /// guarda sus propios nombres: `claude-vscode`, `claude-desktop`).
    pub entrypoint: Option<String>,
    pub started_ms: Option<i64>,
    pub pr_number: Option<u64>,
    /// Modelo con el que trabaja la sesión, si el harness lo registra.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Prompts humanos (sin tool results ni mensajes de sistema).
    pub prompts: u32,
    pub has_messages: bool,
    /// Sesión que no se lista (subagente, hilo hijo, archivada…). Se conserva
    /// en el índice para no releer su archivo en cada pasada.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// Según lo último que guardó el harness, hay un turno en curso. Solo
    /// cuenta para sesiones abiertas: si el proceso murió a medias, se queda
    /// así. Claude no lo usa (publica su estado aparte).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub busy: bool,
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

/// Analizador de un harness: recibe cada línea completa (sin salto de línea) y
/// actualiza la sesión.
pub type LineFn = fn(&[u8], &mut SessionMeta);

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Indexa (o actualiza) un `.jsonl` de sesión. `prev` es la entrada del índice
/// anterior. El id inicial es el nombre del archivo sin extensión; `on_line`
/// puede cambiarlo si el harness lo guarda dentro.
pub fn index_jsonl(path: &Path, prev: Option<&SessionMeta>, harness: Harness, on_line: LineFn) -> std::io::Result<SessionMeta> {
    let md = std::fs::metadata(path)?;
    let size = md.len();
    let mtime = mtime_ms(&md);
    if let Some(p) = prev
        && p.size == size && p.mtime_ms == mtime && p.file == path {
            return Ok(p.clone());
        }

    let mut file = File::open(path)?;

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
            let consumed = parse_lines(&buf[fp_len..], &mut meta, on_line);
            advance(&mut meta, &buf, fp_len + consumed, fp_start);
            return Ok(meta);
        }
        file.seek(SeekFrom::Start(0))?;
    }

    let mut buf = Vec::with_capacity(size as usize);
    file.read_to_end(&mut buf)?;
    let id = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut meta = SessionMeta { harness, id, file: path.to_path_buf(), size, mtime_ms: mtime, ..Default::default() };
    let consumed = parse_lines(&buf, &mut meta, on_line);
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

/// Pasa a `on_line` las líneas completas de `buf`; devuelve los bytes consumidos.
fn parse_lines(buf: &[u8], meta: &mut SessionMeta, on_line: LineFn) -> usize {
    let mut pos = 0;
    while let Some(nl) = memchr::memchr(b'\n', &buf[pos..]) {
        let line = &buf[pos..pos + nl];
        pos += nl + 1;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.len() < 2 {
            continue;
        }
        on_line(line, meta);
    }
    pos
}

pub fn push_link(meta: &mut SessionMeta, link: PeerLink) {
    meta.links.push(link);
    if meta.links.len() > MAX_LINKS {
        let extra = meta.links.len() - MAX_LINKS;
        meta.links.drain(..extra);
    }
}

pub fn set_text(slot: &mut Option<String>, value: Option<String>) {
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
pub fn collapse(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Analizador de prueba: cuenta líneas y toma el id de `id=…`.
    fn count_lines(line: &[u8], meta: &mut SessionMeta) {
        meta.prompts += 1;
        if let Some(id) = line.strip_prefix(b"id=") {
            meta.id = String::from_utf8_lossy(id).into_owned();
        }
    }

    fn tmp(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("claunch-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn index_is_incremental_and_tags_the_harness() {
        let p = tmp("generico.jsonl", "id=abc\r\nuno\n\ndos\nsin terminar");
        let m1 = index_jsonl(&p, None, Harness::Pi, count_lines).unwrap();
        assert_eq!(m1.harness, Harness::Pi);
        assert_eq!(m1.id, "abc");
        // La línea vacía y la que no termina en salto no cuentan.
        assert_eq!(m1.prompts, 3);
        assert_eq!(m1.indexed_to, "id=abc\r\nuno\n\ndos\n".len() as u64);

        // Sin cambios devuelve lo anterior tal cual.
        assert_eq!(index_jsonl(&p, Some(&m1), Harness::Pi, count_lines).unwrap(), m1);

        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        writeln!(f, " ahora sí\ntres").unwrap();
        drop(f);
        let mut stale = m1.clone();
        stale.mtime_ms -= 1;
        let inc = index_jsonl(&p, Some(&stale), Harness::Pi, count_lines).unwrap();
        let full = index_jsonl(&p, None, Harness::Pi, count_lines).unwrap();
        assert_eq!(inc.prompts, 5);
        assert_eq!(inc, full);
    }

    #[test]
    fn id_defaults_to_file_stem() {
        let p = tmp("0f6c2b1e.jsonl", "uno\n");
        assert_eq!(index_jsonl(&p, None, Harness::Codex, count_lines).unwrap().id, "0f6c2b1e");
    }

    #[test]
    fn clean_prompt_rules() {
        assert_eq!(clean_prompt("<local-command-stdout>x</local-command-stdout>"), None);
        assert_eq!(clean_prompt("<bash-input>ls -la</bash-input>").as_deref(), Some("! ls -la"));
        assert_eq!(clean_prompt("  a\n\nb ").as_deref(), Some("a b"));
    }

    #[test]
    fn old_index_entries_default_to_claude() {
        let m: SessionMeta = serde_json::from_str(
            r#"{"id":"s","file":"s.jsonl","size":1,"mtime_ms":2,"indexed_to":0,"fingerprint":0,"cwd":null,"custom_title":null,
                "ai_title":null,"agent_name":null,"first_prompt":null,"last_prompt":null,"git_branch":null,"entrypoint":null,
                "started_ms":null,"pr_number":null,"prompts":0,"has_messages":false}"#,
        )
        .unwrap();
        assert_eq!(m.harness, Harness::Claude);
        assert_eq!(m.model, None);
    }
}
