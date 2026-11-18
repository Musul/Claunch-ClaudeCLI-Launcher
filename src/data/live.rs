//! Sesiones en ejecución.
//!
//! Claude Code (CLI, VS Code y Desktop) registra cada proceso en
//! `~/.claude/sessions/<pid>.json`, con su estado. Un archivo puede quedar
//! huérfano si el proceso murió, así que se valida que el pid siga vivo y que
//! su hora de creación coincida con `procStart` (evita pids reciclados).
//! `procStart` es un FILETIME en Windows y, en Linux, la salida de
//! `ps -o lstart= -p <pid>` en UTC (`Wed Nov 11 15:59:02 2026`).
//! Los archivos `.key` de esa carpeta son secretos: nunca se abren.
//!
//! Los demás harness no publican sus procesos. Para ellos, `claunch run` deja
//! un aviso en `<datos de claunch>/live/<pid>.json` mientras el proceso vive,
//! con un `procStart` propio (en Linux, arranque del sistema y ticks, que no
//! dependen del reloj). claunch sabe así que la sesión está abierta; si
//! trabaja o no lo dice lo último que guardó el harness, y si espera un
//! permiso no se sabe. Las sesiones abiertas fuera de claunch no se ven.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::harness::Harness;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct LiveSession {
    pub harness: Harness,
    pub pid: u32,
    pub session_id: String,
    pub cwd: String,
    pub entrypoint: String,
    /// `busy` (trabajando), `idle`, `waiting` (espera al usuario) o vacío si
    /// solo se sabe que está abierta.
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
                harness: Harness::Claude,
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

// ---------------------------------------------------------------- avisos de claunch

/// Sesión abierta por `claunch run` con un harness que no publica su estado.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub harness: Harness,
    /// Proceso del harness.
    pub pid: u32,
    /// Hora de creación del proceso, en el formato de [`process_alive`].
    #[serde(rename = "procStart", default)]
    pub proc_start: Option<String>,
    /// Id de la sesión: se conoce al arrancar (reanudada o con id fijado) o lo
    /// apunta después la TUI, cuando aparece la sesión nueva de este proceso.
    #[serde(rename = "sessionId", default)]
    pub session_id: Option<String>,
    /// Sesiones nuevas que pudieron ser de este proceso o de otro. La TUI las
    /// apunta para no atribuírselas más tarde, cuando el otro ya no esté.
    #[serde(rename = "notMine", default, skip_serializing_if = "Vec::is_empty")]
    pub not_mine: Vec<String>,
    pub cwd: String,
    /// La misma carpeta sin enlaces simbólicos: es la que registran los
    /// harness que guardan su `cwd` real. Solo si difiere de `cwd`.
    #[serde(rename = "cwdReal", default, skip_serializing_if = "Option::is_none")]
    pub cwd_real: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(rename = "startedAt", default)]
    pub started_ms: i64,
}

impl Marker {
    /// `true` si `cwd` (el de una sesión) es la carpeta donde se abrió esta,
    /// se escriba con la ruta lógica o con la real.
    pub fn in_cwd(&self, cwd: &str) -> bool {
        let key = crate::paths::key(Path::new(cwd));
        [Some(&self.cwd), self.cwd_real.as_ref()].into_iter().flatten().any(|c| crate::paths::key(Path::new(c)) == key)
    }
}

/// Avisos de la carpeta de datos de este proceso.
pub fn markers_dir() -> PathBuf {
    crate::paths::data_dir().join("live")
}

/// Un aviso ilegible más viejo que esto se borra (el escritor es atómico: no
/// hay lecturas a medias que respetar).
const UNREADABLE_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// Deja en `dir` el aviso de una sesión recién arrancada; devuelve su ruta
/// para borrarlo al salir. Si no se puede escribir, la sesión no se verá
/// abierta. `started_ms` es la hora tomada justo antes de arrancar el proceso:
/// ninguna sesión suya puede ser anterior.
pub fn publish(dir: &Path, spec: &crate::launch::RunSpec, pid: u32, started_ms: i64) -> Option<PathBuf> {
    let cwd = spec.cwd.to_string_lossy().into_owned();
    let cwd_real = std::fs::canonicalize(&spec.cwd)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|real| crate::paths::key(Path::new(real)) != crate::paths::key(&spec.cwd));
    let marker = Marker {
        harness: spec.harness,
        pid,
        proc_start: process_started(pid),
        session_id: spec.session_id.clone(),
        not_mine: Vec::new(),
        cwd,
        cwd_real,
        title: spec.title.clone(),
        started_ms,
    };
    let path = dir.join(format!("{pid}.json"));
    match write_marker(&path, &marker) {
        Ok(()) => Some(path),
        Err(e) => {
            crate::warn_log!("run: no se pudo publicar la sesión abierta en {}: {e}", path.display());
            None
        }
    }
}

fn write_marker(path: &Path, marker: &Marker) -> anyhow::Result<()> {
    crate::util::write_atomic(path, &serde_json::to_vec(marker)?)
}

/// Guarda en su aviso lo que la TUI ha averiguado de un proceso (su sesión, o
/// las que no son suyas). Si el proceso ya salió y borró el aviso, no lo
/// recrea; si sale justo ahora, el aviso reaparece y lo quita la siguiente pasada.
pub fn save(dir: &Path, marker: &Marker) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let path = dir.join(format!("{}.json", marker.pid));
    if !path.is_file() {
        return;
    }
    // Sin poder guardarlo se recalcula en cada pasada: un solo aviso en el log.
    if let Err(e) = write_marker(&path, marker)
        && !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        crate::warn_log!("live: no se pudo actualizar {}: {e}", path.display());
    }
}

/// Avisos cuyo proceso sigue vivo. Los huérfanos (proceso muerto o pid
/// reciclado) se borran.
pub fn scan_markers(dir: &Path) -> Vec<Marker> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    for e in entries.flatten() {
        let path = e.path();
        let ext = path.extension().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
        let old = || e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|age| age > UNREADABLE_AFTER);
        // Temporal de una escritura que se quedó a medias (proceso matado).
        if ext.starts_with("tmp-") && old() {
            let _ = std::fs::remove_file(&path);
        }
        if ext != "json" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        match serde_json::from_str::<Marker>(&text) {
            Ok(m) if process_alive(m.pid, m.proc_start.as_deref()) => out.push(m),
            Ok(_) => {
                let _ = std::fs::remove_file(&path);
            }
            // Ilegible (truncado, o de una versión que esta no entiende): se
            // deja un rato por si es de otro claunch y luego se borra, con un
            // solo aviso. Sin esto se registraría en cada pasada.
            Err(err) => {
                if old() && std::fs::remove_file(&path).is_ok() {
                    crate::warn_log!("live/{}: aviso ilegible, borrado: {err}", e.file_name().to_string_lossy());
                }
            }
        }
    }
    out.sort_by_key(|m| (m.started_ms, m.pid));
    out
}

/// Hora de creación de un proceso en el formato que valida [`process_alive`]
/// (FILETIME en Windows).
#[cfg(windows)]
pub fn process_started(pid: u32) -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: llamadas Win32 con handle propio que se cierra siempre.
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut c, mut e, mut k, mut u) = (zero, zero, zero, zero);
        let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0;
        CloseHandle(h);
        ok.then(|| ((u64::from(c.dwHighDateTime) << 32) | u64::from(c.dwLowDateTime)).to_string())
    }
}

/// Identidad de un proceso para los avisos de claunch, en un formato que
/// valida [`process_alive`]: arranque del sistema y `starttime` en ticks
/// (`ticks:<boot_id>:<n>`). No usa la hora: la que da `ps` se mueve si el
/// reloj salta (WSL o una VM tras suspender) y daría por muerto a un proceso vivo.
#[cfg(target_os = "linux")]
pub fn process_started(pid: u32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, start_ticks) = parse_stat(&stat)?;
    Some(format!("{TICKS}{}:{start_ticks}", boot_id().unwrap_or_default()))
}

/// Prefijo del `procStart` propio de claunch en Linux.
#[cfg(target_os = "linux")]
const TICKS: &str = "ticks:";

/// Identificador de este arranque del sistema.
#[cfg(target_os = "linux")]
fn boot_id() -> Option<&'static str> {
    static ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ID.get_or_init(|| std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
        .as_deref()
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn process_started(_pid: u32) -> Option<String> {
    None
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
/// `proc_start`: el de Claude Code (`ps -o lstart=` en UTC) o el de los avisos
/// de claunch ([`process_started`]). Lee `/proc`, sin lanzar procesos.
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
    if let Some(own) = proc_start.and_then(|s| s.strip_prefix(TICKS)) {
        // Mismo arranque del sistema (si se pudo leer) y mismos ticks: exacto.
        let (boot, ticks) = own.rsplit_once(':').unwrap_or(("", own));
        return (boot.is_empty() || boot_id().is_none_or(|b| b == boot)) && ticks.parse::<u64>().ok() == Some(start_ticks);
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

    #[test]
    fn markers_of_dead_processes_are_dropped() {
        let dir = std::env::temp_dir().join("claunch-tests").join(format!("live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let me = std::process::id();
        let marker = |pid: u32, proc_start: Option<String>, started_ms: i64| Marker {
            harness: Harness::Codex,
            pid,
            proc_start,
            session_id: Some("abc".into()),
            cwd: "/r/app".into(),
            title: "App".into(),
            started_ms,
            ..Default::default()
        };
        let write = |name: &str, m: &Marker| std::fs::write(dir.join(name), serde_json::to_vec(m).unwrap()).unwrap();
        write("vivo.json", &marker(me, process_started(me), 20));
        // Mismo pid con otra hora de creación: un pid reciclado.
        write("reciclado.json", &marker(me, Some(if cfg!(windows) { "1".into() } else { "Thu Jan  1 00:00:00 2015".to_string() }), 10));
        write("muerto.json", &marker(u32::MAX - 1, None, 5));
        std::fs::write(dir.join("roto.json"), "{").unwrap();
        std::fs::write(dir.join("9.tmp-123"), "{").unwrap();
        std::fs::write(dir.join("otro.txt"), "x").unwrap();

        let found = scan_markers(&dir);
        if cfg!(any(windows, target_os = "linux")) {
            assert_eq!(found, vec![marker(me, process_started(me), 20)]);
            assert!(process_started(me).is_some());
            assert!(!dir.join("reciclado.json").exists() && !dir.join("muerto.json").exists());
        }
        // Un aviso ilegible reciente se respeta (puede ser de otra versión), igual
        // que el temporal de una escritura en curso; otros archivos, siempre.
        assert!(dir.join("roto.json").exists() && dir.join("9.tmp-123").exists() && dir.join("otro.txt").exists());
        // Pasado un rato, el ilegible y el temporal abandonado se borran.
        for name in ["roto.json", "9.tmp-123"] {
            let f = std::fs::File::options().write(true).open(dir.join(name)).unwrap();
            f.set_modified(std::time::SystemTime::now() - UNREADABLE_AFTER * 2).unwrap();
        }
        scan_markers(&dir);
        assert!(!dir.join("roto.json").exists() && !dir.join("9.tmp-123").exists() && dir.join("otro.txt").exists());
        assert!(scan_markers(&dir.join("no-existe")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_updates_a_marker_but_never_recreates_it() {
        let dir = std::env::temp_dir().join("claunch-tests").join(format!("live-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut m = Marker { harness: Harness::Opencode, pid: 77, cwd: "/r/app".into(), started_ms: 5, ..Default::default() };
        let path = dir.join("77.json");
        // El proceso ya salió (no hay aviso): no se recrea.
        save(&dir, &m);
        assert!(!path.exists());
        // Con el aviso en su sitio, se le apunta lo averiguado.
        write_marker(&path, &m).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("notMine"));
        m.session_id = Some("ses_1".into());
        m.not_mine = vec!["ses_0".into()];
        save(&dir, &m);
        let back: Marker = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back, m);
        // Solo queda el aviso: el temporal de la escritura no se confunde con otro.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_matches_logical_and_real_cwd() {
        let m = Marker {
            harness: Harness::Codex,
            pid: 1,
            cwd: "/home/ana/work/app".into(),
            cwd_real: Some("/data/work/app".into()),
            ..Default::default()
        };
        assert!(m.in_cwd("/home/ana/work/app/"));
        assert!(m.in_cwd("/data/work/app"));
        assert!(!m.in_cwd("/data/work/api"));
    }

    #[test]
    fn publish_writes_next_to_the_given_dir() {
        let dir = std::env::temp_dir().join("claunch-tests").join(format!("publish-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cwd = std::env::temp_dir();
        let spec = crate::launch::RunSpec {
            token: "t".into(),
            cwd: cwd.clone(),
            harness: Harness::Pi,
            program: "pi".into(),
            args: Vec::new(),
            title: "App".into(),
            label: "app".into(),
            session_id: Some("abc".into()),
        };
        let me = std::process::id();
        let path = publish(&dir, &spec, me, 1_234).expect("aviso escrito");
        assert_eq!(path, dir.join(format!("{me}.json")));
        let m: Marker = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((m.harness, m.pid, m.session_id.as_deref(), m.started_ms), (Harness::Pi, me, Some("abc"), 1_234));
        assert!(m.in_cwd(&cwd.to_string_lossy()));
        if cfg!(any(windows, target_os = "linux")) {
            assert_eq!(scan_markers(&dir).len(), 1);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_json_shape() {
        let m: Marker = serde_json::from_str(r#"{"harness":"pi","pid":7,"cwd":"/r"}"#).unwrap();
        assert_eq!((m.harness, m.pid, m.session_id.as_deref(), m.proc_start.as_deref(), m.started_ms), (Harness::Pi, 7, None, None, 0));
        let text = serde_json::to_string(&m).unwrap();
        assert!(text.contains("\"sessionId\":null") && text.contains("\"startedAt\":0"), "{text}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn own_process_is_alive() {
        let me = std::process::id();
        assert!(process_alive(me, None));
        // El de los avisos de claunch: arranque del sistema y ticks, sin reloj.
        let own = process_started(me).expect("identidad del propio proceso");
        assert!(own.starts_with(TICKS) && process_alive(me, Some(&own)), "{own}");
        let (head, ticks) = own.rsplit_once(':').unwrap();
        let other = ticks.parse::<u64>().unwrap() + 1;
        assert!(!process_alive(me, Some(&format!("{head}:{other}"))), "otros ticks: pid reciclado");
        assert!(process_alive(me, Some(&format!("{TICKS}:{ticks}"))), "sin id de arranque bastan los ticks");
        if boot_id().is_some() {
            assert!(!process_alive(me, Some(&format!("{TICKS}otro-arranque:{ticks}"))), "otro arranque del sistema");
        }
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
