//! Uso del plan de Claude (Pro, Max, Team…): ventanas de 5 horas y de 7 días.
//!
//! Fuente: el mismo endpoint que consulta `/usage` de Claude Code
//! (`GET https://api.anthropic.com/api/oauth/usage`), con el token OAuth que
//! Claude Code guarda en `~/.claude/.credentials.json`.
//!
//! - El token solo vive en memoria durante la consulta: nunca se registra,
//!   se muestra ni se escribe en disco.
//! - Solo lectura: claunch no renueva el token (rotaría las credenciales de
//!   Claude Code). Si venció, se pide abrir Claude para que lo renueve.
//! - Una consulta por minuto entre todas las ventanas de claunch: la última
//!   lectura (sin token) se comparte en `usage.json`, en la carpeta de datos
//!   de claunch (`%LOCALAPPDATA%\claunch` o `~/.local/share/claunch`).
//!   Con 429 respeta `Retry-After`, también para las demás ventanas.
//! - El endpoint no está documentado: si cambia, la barra muestra «sin datos»
//!   y el motivo queda en el log.
//! - Se apaga con `general.show_usage = false`.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::WorkerMsg;

const URL: &str = "https://api.anthropic.com/api/oauth/usage";
const BETA: &str = "oauth-2025-04-20";
pub const EVERY: Duration = Duration::from_secs(60);
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// Un límite del plan (ventana de 5 h, semanal, semanal de un modelo…).
#[derive(Debug, Clone, PartialEq)]
pub struct Limit {
    /// Rótulo largo («sesión (5 h)», «semana · Fable»).
    pub label: String,
    /// Rótulo corto para la cabecera («5 h», «semana»).
    pub short: String,
    /// Porcentaje usado, 0–100.
    pub pct: f64,
    pub resets_ms: Option<i64>,
    /// `false`: el límite existe pero hoy no aplica (p. ej. modelo no usado).
    pub active: bool,
    /// Orden: 0 = 5 h, 1 = semanal general, 2 = semanal por modelo, 3 = otros.
    pub rank: u8,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Usage {
    /// Ordenados: activos primero, luego por `rank` y por porcentaje.
    pub limits: Vec<Limit>,
    /// `max`, `pro`, `team`… según las credenciales.
    pub plan: Option<String>,
    pub fetched_ms: i64,
}

impl Usage {
    /// Nombre del plan para mostrar («Max», «Pro»…).
    pub fn plan_label(&self) -> Option<String> {
        let p = self.plan.as_deref()?.trim();
        let mut c = p.chars();
        let first = c.next()?;
        Some(first.to_uppercase().chain(c).collect())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum UsageState {
    Disabled,
    Loading,
    Ready(Usage),
    /// `last`: último dato bueno, si lo hubo (se sigue mostrando atenuado).
    Failed { reason: String, last: Option<Usage> },
}

// ---------------------------------------------------------------- credenciales

#[derive(Deserialize)]
struct CredFile {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuth>,
}

#[derive(Deserialize)]
struct OAuth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at: Option<i64>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
}

/// Token de acceso. Sin `Debug` ni `Display` para que no acabe en un log.
struct Token(String);

struct Creds {
    token: Token,
    plan: Option<String>,
}

impl std::fmt::Debug for Creds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Creds").field("token", &"<oculto>").field("plan", &self.plan).finish()
    }
}

fn read_creds(claude_home: &Path, now_ms: i64) -> Result<Creds, String> {
    let path = claude_home.join(".credentials.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("sin sesión de Claude: ejecuta claude e inicia sesión".into());
        }
        Err(e) => return Err(format!("no se pudo leer .credentials.json: {}", e.kind())),
    };
    parse_creds(&text, now_ms)
}

fn parse_creds(text: &str, now_ms: i64) -> Result<Creds, String> {
    // El error de serde puede citar el texto: se descarta para no filtrar nada.
    let file: CredFile = serde_json::from_str(text).map_err(|_| ".credentials.json ilegible".to_string())?;
    let Some(o) = file.oauth else {
        return Err("Claude no usa suscripción (clave de API): no hay límites de plan".into());
    };
    let Some(token) = o.access_token.filter(|t| !t.is_empty()) else {
        return Err("sin token de suscripción: ejecuta claude e inicia sesión".into());
    };
    if o.expires_at.is_some_and(|e| e <= now_ms) {
        return Err("sesión de Claude vencida: abre claude para renovarla".into());
    }
    Ok(Creds { token: Token(token), plan: o.subscription_type })
}

// ---------------------------------------------------------------- respuesta

fn ts_ms(v: Option<&serde_json::Value>) -> Option<i64> {
    v?.as_str()?.parse::<jiff::Timestamp>().ok().map(|t| t.as_millisecond())
}

/// Formato nuevo: `limits: [{kind, group, percent, resets_at, scope, is_active}]`.
fn from_limits(arr: &[serde_json::Value]) -> Vec<Limit> {
    arr.iter()
        .filter_map(|l| {
            let pct = l.get("percent").or_else(|| l.get("utilization"))?.as_f64()?;
            let kind = l.get("kind").and_then(|k| k.as_str()).unwrap_or("");
            let group = l.get("group").and_then(|k| k.as_str()).unwrap_or("");
            let scope = l.get("scope").filter(|s| !s.is_null());
            let model = scope
                .and_then(|s| s.pointer("/model/display_name").or_else(|| s.pointer("/model/id")))
                .and_then(|m| m.as_str())
                .or_else(|| scope.and_then(|s| s.pointer("/surface")).and_then(|m| m.as_str()));
            let (label, short, rank) = if kind.contains("five_hour") || kind.contains("session") || group == "session" || group == "five_hour" {
                ("sesión (5 h)".to_string(), "5 h".to_string(), 0)
            } else if group == "weekly" || kind.starts_with("weekly") {
                match model {
                    Some(m) => (format!("semana · {m}"), format!("semana {m}"), 2),
                    None => ("semana".to_string(), "semana".to_string(), 1),
                }
            } else {
                let k = if kind.is_empty() { group } else { kind };
                (k.replace('_', " "), k.replace('_', " "), 3)
            };
            Some(Limit {
                label,
                short,
                pct: pct.clamp(0.0, 100.0),
                resets_ms: ts_ms(l.get("resets_at")),
                active: l.get("is_active").and_then(|a| a.as_bool()).unwrap_or(true),
                rank,
            })
        })
        .collect()
}

/// Formato anterior: `five_hour`, `seven_day`, `seven_day_<modelo>` con `utilization`.
fn from_windows(obj: &serde_json::Map<String, serde_json::Value>) -> Vec<Limit> {
    obj.iter()
        .filter_map(|(k, v)| {
            let pct = v.get("utilization")?.as_f64()?;
            let (label, short, rank) = match k.as_str() {
                "five_hour" => ("sesión (5 h)".to_string(), "5 h".to_string(), 0),
                "seven_day" => ("semana".to_string(), "semana".to_string(), 1),
                "seven_day_oauth_apps" => return None,
                other => {
                    let m = other.strip_prefix("seven_day_")?;
                    (format!("semana · {m}"), format!("semana {m}"), 2)
                }
            };
            Some(Limit { label, short, pct: pct.clamp(0.0, 100.0), resets_ms: ts_ms(v.get("resets_at")), active: true, rank })
        })
        .collect()
}

pub fn parse_usage(body: &str, plan: Option<String>, now_ms: i64) -> Result<Usage, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("respuesta no es JSON: {e}"))?;
    let obj = v.as_object().ok_or("respuesta inesperada (no es un objeto)")?;
    let mut limits = obj.get("limits").and_then(|l| l.as_array()).map(|a| from_limits(a)).unwrap_or_default();
    // Completa con el formato anterior lo que `limits` no traiga (p. ej. la ventana de 5 h).
    for old in from_windows(obj) {
        if !limits.iter().any(|l| l.rank == old.rank && (old.rank < 2 || l.label == old.label)) {
            limits.push(old);
        }
    }
    if limits.is_empty() {
        return Err("la respuesta no trae límites (¿cambió el formato?)".into());
    }
    limits.sort_by(|a, b| b.active.cmp(&a.active).then(a.rank.cmp(&b.rank)).then(b.pct.total_cmp(&a.pct)));
    Ok(Usage { limits, plan, fetched_ms: now_ms })
}

impl Usage {
    /// Límites para la cabecera: los activos (o todos si ninguno lo está).
    pub fn headline(&self) -> Vec<&Limit> {
        let active: Vec<&Limit> = self.limits.iter().filter(|l| l.active).collect();
        if active.is_empty() { self.limits.iter().collect() } else { active }
    }
}

// ---------------------------------------------------------------- consulta

enum Fail {
    /// Motivo para mostrar y espera antes de reintentar.
    Retry(String, Duration),
    /// 429: la espera vale para todos los procesos de claunch.
    RateLimited(String, Duration),
}

/// Última lectura compartida entre procesos de claunch (solo datos de uso).
#[derive(Serialize, Deserialize, Default)]
struct Cache {
    #[serde(default)]
    fetched_ms: i64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    plan: Option<String>,
    /// Hasta cuándo no consultar (tras un 429) y por qué.
    #[serde(default)]
    blocked_until_ms: i64,
    #[serde(default)]
    reason: String,
}

fn cache_path() -> PathBuf {
    crate::paths::data_dir().join("usage.json")
}

fn load_cache() -> Cache {
    std::fs::read(cache_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn save_cache(c: &Cache) {
    let r = serde_json::to_vec(c).map_err(anyhow::Error::from).and_then(|b| crate::util::write_atomic(&cache_path(), &b));
    if let Err(e) = r {
        crate::warn_log!("uso del plan: no se pudo guardar la caché: {e}");
    }
}

enum Outcome {
    Fresh(Usage),
    Failed { reason: String, last: Option<Usage> },
}

/// Una lectura: de la caché si tiene menos de un minuto o hay una espera
/// vigente por 429; si no, de la API. Devuelve también cuánto esperar.
fn poll(agent: &ureq::Agent, claude_home: &Path) -> (Outcome, Duration) {
    let now = crate::util::now_ms();
    let mut cache = load_cache();
    let cached = (!cache.body.is_empty()).then(|| parse_usage(&cache.body, cache.plan.clone(), cache.fetched_ms).ok()).flatten();
    let every_ms = EVERY.as_millis() as i64;
    let age = now - cache.fetched_ms;
    if let Some(u) = &cached
        && (0..every_ms - 2_000).contains(&age)
    {
        return (Outcome::Fresh(u.clone()), Duration::from_millis((every_ms - age) as u64));
    }
    if cache.blocked_until_ms > now {
        let wait = Duration::from_millis((cache.blocked_until_ms - now) as u64).min(EVERY);
        return (Outcome::Failed { reason: cache.reason.clone(), last: cached }, wait);
    }
    match fetch_body(agent, claude_home) {
        Ok((body, plan, t)) => match parse_usage(&body, plan.clone(), t) {
            Ok(u) => {
                save_cache(&Cache { fetched_ms: t, body, plan, blocked_until_ms: 0, reason: String::new() });
                (Outcome::Fresh(u), EVERY)
            }
            Err(reason) => (Outcome::Failed { reason, last: cached }, EVERY * 2),
        },
        Err(Fail::RateLimited(reason, wait)) => {
            cache.blocked_until_ms = now + wait.as_millis() as i64;
            cache.reason = reason.clone();
            save_cache(&cache);
            (Outcome::Failed { reason, last: cached }, wait.min(EVERY))
        }
        Err(Fail::Retry(reason, wait)) => (Outcome::Failed { reason, last: cached }, wait),
    }
}

fn agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig};
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .http_status_as_error(false)
        .user_agent(concat!("claunch/", env!("CARGO_PKG_VERSION")))
        // Almacén de certificados del sistema: funciona tras proxies corporativos.
        .tls_config(TlsConfig::builder().root_certs(RootCerts::PlatformVerifier).build())
        .build()
        .into()
}

/// Cuerpo JSON de la respuesta (no contiene secretos), plan y hora de la consulta.
fn fetch_body(agent: &ureq::Agent, claude_home: &Path) -> Result<(String, Option<String>, i64), Fail> {
    let now = crate::util::now_ms();
    let creds = read_creds(claude_home, now).map_err(|r| Fail::Retry(r, EVERY))?;
    let auth = format!("Bearer {}", creds.token.0);
    let resp = agent
        .get(URL)
        .header("Authorization", &auth)
        .header("anthropic-beta", BETA)
        .header("Accept", "application/json")
        .call();
    drop(auth);
    let mut resp = match resp {
        Ok(r) => r,
        Err(e) => return Err(Fail::Retry(format!("sin conexión con api.anthropic.com ({})", short_err(&e)), EVERY)),
    };
    let status = resp.status().as_u16();
    match status {
        200 => {}
        401 | 403 => return Err(Fail::Retry("sesión de Claude vencida o sin permiso: abre claude para renovarla".into(), EVERY * 2)),
        429 => {
            let wait = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or(EVERY * 5)
                .clamp(EVERY, MAX_BACKOFF);
            return Err(Fail::RateLimited(format!("límite de consultas: reintento en {} min", wait.as_secs().div_ceil(60)), wait));
        }
        s => return Err(Fail::Retry(format!("api.anthropic.com respondió {s}"), EVERY * 2)),
    }
    let body = resp
        .body_mut()
        .with_config()
        .limit(256 * 1024)
        .read_to_string()
        .map_err(|e| Fail::Retry(format!("respuesta ilegible ({})", short_err(&e)), EVERY))?;
    Ok((body, creds.plan, now))
}

/// `claunch usage [--json]`: muestra el uso del plan en la terminal (usa la
/// lectura compartida si tiene menos de un minuto).
pub fn cli(args: &[String]) -> std::process::ExitCode {
    let home = crate::paths::claude_home();
    let u = match poll(&agent(), &home) {
        (Outcome::Fresh(u), _) => u,
        (Outcome::Failed { reason, last }, _) => {
            eprintln!("claunch usage: {reason}");
            match last {
                Some(u) => {
                    eprintln!("(último dato: {})", crate::util::relative_time(u.fetched_ms, crate::util::now_ms()));
                    u
                }
                None => return std::process::ExitCode::from(1),
            }
        }
    };
    if args.iter().any(|a| a == "--json") {
        println!("{}", load_cache().body);
        return std::process::ExitCode::SUCCESS;
    }
    let line = |w: &Limit| {
        let filled = ((w.pct / 5.0).round() as usize).min(20);
        let reset = w
            .resets_ms
            .and_then(|r| jiff::Timestamp::from_millisecond(r).ok())
            .map(|t| {
                const DAYS: [&str; 7] = ["lun", "mar", "mié", "jue", "vie", "sáb", "dom"];
                let z = t.to_zoned(jiff::tz::TimeZone::system());
                format!("  reinicia {} {} {:02}:{:02}", DAYS[z.weekday().to_monday_zero_offset() as usize], z.day(), z.hour(), z.minute())
            })
            .unwrap_or_default();
        let off = if w.active { "" } else { "  (no aplica ahora)" };
        println!("{:<18} {}{} {:>3.0}%{reset}{off}", w.label, "█".repeat(filled), "░".repeat(20 - filled), w.pct);
    };
    if let Some(p) = u.plan_label() {
        println!("Plan {p}");
    }
    for l in &u.limits {
        line(l);
    }
    std::process::ExitCode::SUCCESS
}

fn short_err(e: &ureq::Error) -> String {
    crate::util::truncate(&e.to_string(), 60)
}

/// Hilo que publica el uso al arrancar y luego cada minuto.
pub fn spawn(claude_home: PathBuf, tx: Sender<WorkerMsg>) {
    let spawned = std::thread::Builder::new().name("claunch-usage".into()).spawn(move || {
        let agent = agent();
        let mut last: Option<Usage> = None;
        let mut last_reason = String::new();
        loop {
            let (outcome, wait) = poll(&agent, &claude_home);
            let state = match outcome {
                Outcome::Fresh(u) => {
                    last = Some(u.clone());
                    last_reason.clear();
                    UsageState::Ready(u)
                }
                Outcome::Failed { reason, last: cached } => {
                    if reason != last_reason {
                        crate::warn_log!("uso del plan: {reason}");
                        last_reason = reason.clone();
                    }
                    // El dato más nuevo entre el propio y el de la caché.
                    let best = match (cached, last.clone()) {
                        (Some(a), Some(b)) => Some(if a.fetched_ms >= b.fetched_ms { a } else { b }),
                        (a, b) => a.or(b),
                    };
                    UsageState::Failed { reason, last: best }
                }
            };
            if tx.send(WorkerMsg::Usage(state)).is_err() {
                return;
            }
            std::thread::sleep(wait);
        }
    });
    if let Err(e) = spawned {
        crate::warn_log!("no se pudo crear el hilo de uso del plan: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: &str = r#"{
        "five_hour": {"utilization": 42.0, "resets_at": "2026-11-11T18:00:00.123456+00:00"},
        "seven_day": {"utilization": 17.5, "resets_at": "2026-11-15T03:59:59+00:00"},
        "seven_day_oauth_apps": null,
        "seven_day_opus": {"utilization": 63.0, "resets_at": null},
        "extra_usage": {"is_enabled": false}
    }"#;

    // Forma real observada en 2026-11 (recortada).
    const NEW: &str = r#"{
        "five_hour": null,
        "seven_day": {"utilization": 60.0, "resets_at": "2026-11-12T13:00:00.007175+00:00"},
        "limits": [
            {"kind": "weekly_all", "group": "weekly", "percent": 60, "severity": "normal", "resets_at": "2026-11-12T13:00:00.007175+00:00", "scope": null, "is_active": true},
            {"kind": "weekly_scoped", "group": "weekly", "percent": 0, "severity": "normal", "resets_at": "2026-11-12T13:00:00+00:00", "scope": {"model": {"id": null, "display_name": "Fable"}, "surface": null}, "is_active": false}
        ]
    }"#;

    #[test]
    fn parses_old_windows() {
        let u = parse_usage(OLD, Some("max".into()), 1).unwrap();
        let labels: Vec<&str> = u.limits.iter().map(|l| l.short.as_str()).collect();
        assert_eq!(labels, ["5 h", "semana", "semana opus"]);
        assert_eq!(u.limits[0].pct, 42.0);
        assert!(u.limits[0].resets_ms.is_some());
        assert_eq!(u.plan_label().as_deref(), Some("Max"));
    }

    #[test]
    fn parses_new_limits() {
        let u = parse_usage(NEW, None, 1).unwrap();
        assert_eq!(u.limits.len(), 2, "{:?}", u.limits);
        assert_eq!(u.limits[0].short, "semana");
        assert_eq!(u.limits[0].pct, 60.0);
        assert_eq!(u.limits[1].label, "semana · Fable");
        assert!(!u.limits[1].active);
        let head: Vec<&str> = u.headline().iter().map(|l| l.short.as_str()).collect();
        assert_eq!(head, ["semana"]);
    }

    #[test]
    fn rejects_unknown_shape() {
        assert!(parse_usage(r#"{"foo": 1}"#, None, 1).is_err());
        assert!(parse_usage("<html>", None, 1).is_err());
    }

    #[test]
    fn creds_rules() {
        let ok = r#"{"claudeAiOauth":{"accessToken":"t","expiresAt":2000,"subscriptionType":"pro"}}"#;
        let c = parse_creds(ok, 1000).unwrap();
        assert_eq!(c.plan.as_deref(), Some("pro"));
        assert!(parse_creds(ok, 3000).unwrap_err().contains("vencida"));
        assert!(parse_creds(r#"{"other":1}"#, 0).unwrap_err().contains("suscripción"));
        // El error de JSON no cita el contenido del archivo.
        let e = parse_creds(r#"{"claudeAiOauth": SECRETO"#, 0).unwrap_err();
        assert!(!e.contains("SECRETO"));
    }
}
