//! Sesiones en ejecución: `~/.claude/sessions/<pid>.json`.
//!
//! Claude Code (CLI, VS Code y Desktop) registra ahí cada proceso. Un archivo
//! puede quedar huérfano si el proceso murió, así que se valida que el pid siga
//! vivo y que su hora de creación coincida con `procStart` (evita pids reciclados).
//! `procStart` es un FILETIME en Windows y, en Linux, la salida de
//! `ps -o lstart= -p <pid>` en UTC (`Wed Nov 11 15:59:02 2026`).
//! Los archivos `.key` de esa carpeta son secretos: nunca se abren.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LiveSession {
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    pub entrypoint: String,
    /// `busy` (trabajando), `idle` o `waiting` (espera al usuario).
    pub status: String,
    /// Con `waiting`: qué espera («input needed», «dialog open»…).
    pub waiting_for: Option<String>,
    pub name: Option<String>,
    pub started_ms: i64,
    /// Canal de mensajería entre sesiones: tubería `\\.\pipe\LOCAL\cc-msg-…` en
    /// Windows, socket Unix en Linux.
    pub socket: Option<String>,
    /// Id de Remote Control (`session_…`) si la sesión está enlazada.
    pub bridge_id: Option<String>,
    /// Subagentes con actividad reciente (lo rellena el worker).
    pub subagents: usize,
}

impl LiveSession {
    /// Claude está procesando un turno.
    pub fn is_busy(&self) -> bool {
        self.status == "busy"
    }
    /// Claude espera una respuesta o permiso del usuario.
    pub fn is_waiting(&self) -> bool {
        self.status == "waiting"
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
    #[serde(rename = "waitingFor")]
    waiting_for: Option<String>,
    name: Option<String>,
    #[serde(rename = "messagingSocketPath")]
    socket: Option<String>,
    #[serde(rename = "bridgeSessionId")]
    bridge_id: Option<String>,
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
        if !process_alive(raw.pid, raw.proc_start.as_deref()) {
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
                waiting_for: raw.waiting_for.filter(|w| !w.is_empty()),
                name: raw.name.filter(|n| !n.is_empty()),
                started_ms: raw.started_at,
                socket: raw.socket.filter(|s| !s.is_empty()),
                bridge_id: raw.bridge_id.filter(|s| !s.is_empty()),
                subagents: 0,
            },
        );
    }
    out
}

/// `true` si el proceso existe y (si se conoce) nació en `proc_start` (FILETIME).
#[cfg(windows)]
pub fn process_alive(pid: u32, proc_start: Option<&str>) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let created = proc_start.and_then(|s| s.parse::<u64>().ok());
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

/// `true` si el proceso existe, no es zombi y (si se conoce) nació en
/// `proc_start` (`ps -o lstart=` en UTC). Lee `/proc`, sin lanzar procesos.
#[cfg(target_os = "linux")]
pub fn process_alive(pid: u32, proc_start: Option<&str>) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
    let Some((state, start_ticks)) = parse_stat(&stat) else {
        crate::warn_log!("/proc/{pid}/stat ilegible");
        return false;
    };
    if matches!(state, 'Z' | 'X' | 'x') {
        return false;
    }
    let Some(expected) = proc_start.and_then(parse_lstart) else { return true };
    match (boot_time_secs(), clock_ticks()) {
        // `ps` calcula lstart igual: arranque del sistema + ticks / CLK_TCK.
        (Some(btime), Some(hz)) => (btime + (start_ticks / hz) as i64).abs_diff(expected) <= 2,
        // Sin hora de arranque no se puede descartar un pid reciclado: se da por vivo.
        _ => {
            crate::warn_log!("no se pudo leer la hora de arranque del sistema; pid {pid} sin verificar");
            true
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn process_alive(_pid: u32, _proc_start: Option<&str>) -> bool {
    false
}

/// Estado y `starttime` (ticks desde el arranque) de `/proc/<pid>/stat`.
/// El nombre del proceso va entre paréntesis y puede contener espacios.
#[cfg(any(target_os = "linux", test))]
fn parse_stat(stat: &str) -> Option<(char, u64)> {
    let after = &stat[stat.rfind(')')? + 1..];
    let mut fields = after.split_whitespace();
    let state = fields.next()?.chars().next()?;
    // `starttime` es el campo 22; tras el nombre empieza el 3 (estado).
    let start = fields.nth(22 - 4)?.parse().ok()?;
    Some((state, start))
}

/// `Wed Nov 11 15:59:02 2026` (UTC) → segundos Unix.
#[cfg(any(target_os = "linux", test))]
fn parse_lstart(s: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let parts: Vec<&str> = s.split_whitespace().collect();
    let [_, mon, day, hms, year] = parts.as_slice() else { return None };
    let month = MONTHS.iter().position(|m| m == mon)? as i8 + 1;
    let mut t = hms.split(':').map(|x| x.parse::<i8>().ok());
    let (h, m, sec) = (t.next()??, t.next()??, t.next()??);
    let dt = jiff::civil::DateTime::new(year.parse().ok()?, month, day.parse().ok()?, h, m, sec, 0).ok()?;
    Some(dt.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp().as_second())
}

#[cfg(target_os = "linux")]
fn boot_time_secs() -> Option<i64> {
    static BTIME: std::sync::OnceLock<Option<i64>> = std::sync::OnceLock::new();
    *BTIME.get_or_init(|| {
        let text = std::fs::read_to_string("/proc/stat").ok()?;
        text.lines().find_map(|l| l.strip_prefix("btime "))?.trim().parse().ok()
    })
}

#[cfg(target_os = "linux")]
fn clock_ticks() -> Option<u64> {
    // SAFETY: sysconf solo consulta una constante del sistema.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(hz).ok().filter(|h| *h > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proc_stat() {
        let stat = "4242 (node (claude) x) S 1 4242 4242 34816 4242 4194304 1 0 0 0 5 2 0 0 20 0 11 0 987654 0 0";
        assert_eq!(parse_stat(stat), Some(('S', 987654)));
        assert_eq!(parse_stat("1 (x) Z"), None);
    }

    #[test]
    fn parses_ps_lstart() {
        // 2026-11-11T15:59:02Z
        assert_eq!(parse_lstart("Wed Nov 11 15:59:02 2026"), Some(1_794_412_742));
        assert_eq!(parse_lstart("Sun Mar  1 09:05:00 2026"), Some(1_772_355_900));
        assert_eq!(parse_lstart("134388807592906634"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn own_process_is_alive() {
        let me = std::process::id();
        assert!(process_alive(me, None));
        // Lo mismo que escribe Claude Code: `ps -o lstart= -p <pid>` en UTC.
        if let Ok(out) = std::process::Command::new("ps")
            .args(["-o", "lstart=", "-p", &me.to_string()])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .output()
        {
            let lstart = String::from_utf8_lossy(&out.stdout).trim().to_string();
            assert!(process_alive(me, Some(&lstart)), "lstart {lstart:?}");
            assert!(!process_alive(me, Some("Thu Jan  1 00:00:00 2015")));
        }
        assert!(!process_alive(u32::MAX - 1, None));
    }
}
