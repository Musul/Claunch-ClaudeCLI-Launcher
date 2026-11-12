//! Interruptor global de `permissions.defaultMode = "bypassPermissions"` en
//! `~/.claude/settings.json`.
//!
//! Seguridad: con bypass activo, todas las sesiones de Claude Code ejecutan
//! herramientas sin pedir permiso. Por eso: activar exige confirmación, cada
//! escritura deja un respaldo `settings.json.bak-claunch-bypass-<fecha>`, se
//! conservan orden y valores del resto del archivo (puede contener secretos en
//! `env`: nunca se imprimen) y se verifica tras escribir que solo cambió esa clave.

use std::path::PathBuf;

use anyhow::{Context, bail};
use serde_json::{Map, Value};

use crate::paths;

const BYPASS: &str = "bypassPermissions";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BypassStatus {
    pub enabled: bool,
    /// Valor actual de `permissions.defaultMode`, si hay.
    pub mode: Option<String>,
    /// Una política gestionada desactiva el modo bypass.
    pub blocked_by_policy: bool,
}

pub fn settings_path() -> PathBuf {
    paths::claude_home().join("settings.json")
}

fn previous_path() -> PathBuf {
    paths::data_dir().join("bypass-previous.json")
}

fn read_json(path: &std::path::Path) -> anyhow::Result<Option<Value>> {
    match std::fs::read_to_string(path) {
        Ok(t) if t.trim().is_empty() => Ok(Some(Value::Object(Map::new()))),
        Ok(t) => Ok(Some(serde_json::from_str(&t).with_context(|| format!("{} no es JSON válido", path.display()))?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("no se pudo leer {}", path.display())),
    }
}

fn default_mode(v: &Value) -> Option<String> {
    v.get("permissions")?.get("defaultMode")?.as_str().map(String::from)
}

fn policy_blocks() -> bool {
    let mut candidates = vec![paths::claude_home().join("remote-settings.json")];
    #[cfg(windows)]
    candidates.extend([
        PathBuf::from(r"C:\Program Files\ClaudeCode\managed-settings.json"),
        PathBuf::from(r"C:\ProgramData\ClaudeCode\managed-settings.json"),
    ]);
    #[cfg(target_os = "linux")]
    candidates.push(PathBuf::from("/etc/claude-code/managed-settings.json"));
    candidates.iter().any(|p| {
        read_json(p)
            .ok()
            .flatten()
            .and_then(|v| v.get("permissions")?.get("disableBypassPermissionsMode")?.as_str().map(|s| s == "disable"))
            .unwrap_or(false)
    })
}

pub fn status() -> anyhow::Result<BypassStatus> {
    let v = read_json(&settings_path())?.unwrap_or(Value::Object(Map::new()));
    let mode = default_mode(&v);
    Ok(BypassStatus { enabled: mode.as_deref() == Some(BYPASS), mode, blocked_by_policy: policy_blocks() })
}

/// Activa o desactiva. Devuelve la ruta del respaldo (si había archivo).
pub fn set_bypass(on: bool) -> anyhow::Result<Option<PathBuf>> {
    let path = settings_path();
    let original = read_json(&path)?;
    let mut v = original.clone().unwrap_or(Value::Object(Map::new()));
    let Value::Object(root) = &mut v else { bail!("{} no es un objeto JSON", path.display()) };
    let current = default_mode(&Value::Object(root.clone()));

    let perms = root.entry("permissions").or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(perms) = perms else { bail!("`permissions` en {} no es un objeto", path.display()) };

    if on {
        if current.as_deref() == Some(BYPASS) {
            return Ok(None);
        }
        // Recordar el modo anterior para restaurarlo al apagar.
        let prev = serde_json::json!({ "defaultMode": current });
        crate::util::write_atomic(&previous_path(), prev.to_string().as_bytes())?;
        perms.insert("defaultMode".into(), Value::String(BYPASS.into()));
    } else {
        if current.as_deref() != Some(BYPASS) {
            return Ok(None);
        }
        let prev = read_json(&previous_path()).ok().flatten().and_then(|p| p.get("defaultMode")?.as_str().map(String::from));
        match prev.filter(|p| p != BYPASS) {
            Some(p) => {
                perms.insert("defaultMode".into(), Value::String(p));
            }
            None => {
                perms.remove("defaultMode");
            }
        }
    }

    // Respaldo antes de tocar el archivo.
    let backup = if path.exists() {
        let date = jiff::Zoned::now().strftime("%Y%m%d").to_string();
        let mut b = path.with_file_name(format!("settings.json.bak-claunch-bypass-{date}"));
        if b.exists() {
            let time = jiff::Zoned::now().strftime("%H%M%S").to_string();
            b = path.with_file_name(format!("settings.json.bak-claunch-bypass-{date}-{time}"));
        }
        std::fs::copy(&path, &b).with_context(|| format!("no se pudo respaldar en {}", b.display()))?;
        Some(b)
    } else {
        None
    };

    let mut text = serde_json::to_string_pretty(&v)?;
    text.push('\n');
    crate::util::write_atomic(&path, text.as_bytes())?;

    // Verificación en la fuente: solo cambió permissions.defaultMode.
    let after = read_json(&path)?.context("settings.json desapareció tras escribir")?;
    if strip_mode(after.clone()) != strip_mode(original.unwrap_or(Value::Object(Map::new()))) {
        bail!("verificación fallida: cambiaron otras claves; restaura desde {:?}", backup);
    }
    let now_on = default_mode(&after).as_deref() == Some(BYPASS);
    if now_on != on {
        bail!("verificación fallida: defaultMode no quedó como se esperaba");
    }
    crate::info_log!("bypass global {} (respaldo {:?})", if on { "activado" } else { "desactivado" }, backup);
    Ok(backup)
}

/// Copia sin `permissions.defaultMode` (y sin `permissions` si queda vacío).
fn strip_mode(mut v: Value) -> Value {
    if let Some(Value::Object(p)) = v.get_mut("permissions") {
        p.remove("defaultMode");
        if p.is_empty()
            && let Value::Object(root) = &mut v {
                root.remove("permissions");
            }
    }
    v
}

/// `claunch bypass [status|on|off] [--yes]`
pub fn cli(args: &[String]) -> std::process::ExitCode {
    use std::process::ExitCode;
    let action = args.first().map(String::as_str).unwrap_or("status");
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let show = |s: &BypassStatus| {
        println!(
            "bypass global: {}  (permissions.defaultMode = {})",
            if s.enabled { "ACTIVO" } else { "inactivo" },
            s.mode.as_deref().unwrap_or("sin definir")
        );
        if s.blocked_by_policy {
            println!("aviso: una política gestionada desactiva el modo bypass; no tendrá efecto.");
        }
    };
    let result = match action {
        "status" => status().map(|s| show(&s)),
        "on" if !yes => {
            eprintln!(
                "Activar bypass hace que TODAS las sesiones de Claude Code ejecuten herramientas sin pedir permiso.\n\
                 Repite con --yes para confirmarlo: claunch bypass on --yes"
            );
            return ExitCode::from(1);
        }
        "on" | "off" => set_bypass(action == "on").and_then(|b| {
            if let Some(b) = b {
                println!("respaldo: {}", b.display());
            }
            status().map(|s| show(&s))
        }),
        _ => {
            eprintln!("uso: claunch bypass [status|on|off] [--yes]");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("claunch: {e:#}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ignores_only_default_mode() {
        let a: Value = serde_json::from_str(r#"{"env":{"X":"1"},"permissions":{"allow":["a"],"defaultMode":"plan"}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"env":{"X":"1"},"permissions":{"allow":["a"],"defaultMode":"bypassPermissions"}}"#).unwrap();
        assert_eq!(strip_mode(a), strip_mode(b.clone()));
        let c: Value = serde_json::from_str(r#"{"env":{"X":"2"},"permissions":{"allow":["a"]}}"#).unwrap();
        assert_ne!(strip_mode(b), strip_mode(c));
    }

    #[test]
    fn preserves_key_order() {
        let text = r#"{"zeta":1,"alpha":{"b":1,"a":2}}"#;
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), text);
    }
}
