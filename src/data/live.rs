//! Sesiones en ejecución: `~/.claude/sessions/<pid>.json`.
//!
//! Claude Code (CLI, VS Code y Desktop) registra ahí cada proceso. Un archivo
//! puede quedar huérfano si el proceso murió, así que se valida que el pid siga
//! vivo y que su hora de creación coincida con `procStart` (evita pids reciclados).
//! Los archivos `.key` de esa carpeta son secretos: nunca se abren.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq)]
pub struct LiveSession {
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    pub entrypoint: String,
    pub status: String,
    pub name: Option<String>,
    pub started_ms: i64,
}

impl LiveSession {
    pub fn is_busy(&self) -> bool {
        self.status != "idle"
    }
}

#[derive(Deserialize)]
struct Raw {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    #[serde(default)]
    cwd: String,
    #[serde(default)]
    entrypoint: String,
    #[serde(default)]
    status: String,
    name: Option<String>,
    #[serde(rename = "startedAt", default)]
    started_at: i64,
    #[serde(rename = "procStart")]
    proc_start: Option<String>,
}

/// Mapa `session_id → sesión viva`.
pub fn scan(claude_home: &Path) -> HashMap<String, LiveSession> {
    let mut out = HashMap::new();
    let dir = claude_home.join("sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else { return out };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        // Solo `<pid>.json`; jamás `*.key`.
        let Some(stem) = name.strip_suffix(".json") else { continue };
        if stem.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        let raw: Raw = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(err) => {
                crate::warn_log!("sessions/{name}: json inválido: {err}");
                continue;
            }
        };
        let start = raw.proc_start.as_deref().and_then(|s| s.parse::<u64>().ok());
        if !process_alive(raw.pid, start) {
            continue;
        }
        out.insert(
            raw.session_id.clone(),
            LiveSession {
                pid: raw.pid,
                session_id: raw.session_id,
                cwd: raw.cwd,
                entrypoint: raw.entrypoint,
                status: raw.status,
                name: raw.name.filter(|n| !n.is_empty()),
                started_ms: raw.started_at,
            },
        );
    }
    out
}

/// `true` si el proceso existe y (si se conoce) nació en `created` (FILETIME).
#[cfg(windows)]
pub fn process_alive(pid: u32, created: Option<u64>) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: llamadas Win32 con handle propio que se cierra siempre.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let running = GetExitCodeProcess(h, &mut code) != 0 && code == STILL_ACTIVE as u32;
        let mut matches = true;
        if running
            && let Some(expected) = created {
                let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
                let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
                if GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0 {
                    let actual = (u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime);
                    // Tolerancia de 1 s (10^7 unidades de 100 ns).
                    matches = actual.abs_diff(expected) <= 10_000_000;
                }
            }
        CloseHandle(h);
        running && matches
    }
}

#[cfg(not(windows))]
pub fn process_alive(_pid: u32, _created: Option<u64>) -> bool {
    false
}
