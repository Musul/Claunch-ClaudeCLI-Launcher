//! Rutas del sistema y normalización de rutas de Windows.
//!
//! Regla clave: Claude Code compara el `cwd` como texto. La extensión de VS Code
//! guarda `c:\...` (unidad en minúscula) y la CLI desde Git Bash `C:\...`.
//! claunch arranca siempre con la unidad en minúscula para que `/resume` vea
//! las sesiones de ambas superficies (ver docs/diagnostico-resume-mayusculas.md).

use std::path::{Path, PathBuf};

/// Carpeta de datos de Claude Code (`CLAUDE_CONFIG_DIR` o `~/.claude`).
pub fn claude_home() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    home().join(".claude")
}

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// `%APPDATA%\claunch`: configuración y sets (roaming).
pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(home).join("claunch")
}

/// `%LOCALAPPDATA%\claunch`: índice, log y archivos de arranque.
pub fn data_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(home).join("claunch")
}

pub fn launch_dir() -> PathBuf {
    data_dir().join("launch")
}

/// Expande `~` y variables `%VAR%` simples.
pub fn expand(raw: &str) -> PathBuf {
    let mut s = raw.trim().to_string();
    if let Some(rest) = s.strip_prefix('~') {
        s = format!("{}{}", home().display(), rest);
    }
    while let Some(start) = s.find('%') {
        let Some(len) = s[start + 1..].find('%') else { break };
        let name = &s[start + 1..start + 1 + len];
        let value = std::env::var(name).unwrap_or_default();
        s.replace_range(start..start + len + 2, &value);
    }
    PathBuf::from(s)
}

/// Normaliza a la forma con la que claunch arranca Claude:
/// separadores `\`, sin prefijo `\\?\`, sin `\` final y unidad en minúscula.
pub fn normalize(path: &Path) -> PathBuf {
    let mut s = path.to_string_lossy().replace('/', "\\");
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    // Rutas MSYS (/c/Users/...) a Windows.
    let b = s.as_bytes();
    if b.len() >= 3 && b[0] == b'\\' && b[1].is_ascii_alphabetic() && b[2] == b'\\' {
        s = format!("{}:{}", &s[1..2], &s[2..]);
    }
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        let drive = (b[0] as char).to_ascii_lowercase();
        s.replace_range(0..1, &drive.to_string());
    }
    PathBuf::from(s)
}

/// Clave para comparar rutas sin distinguir mayúsculas (como el sistema de archivos).
pub fn key(path: &Path) -> String {
    normalize(path).to_string_lossy().to_lowercase()
}

/// `c:\Users\x` → `/c/Users/x` (para scripts de Git Bash).
pub fn to_msys(path: &Path) -> String {
    let s = normalize(path).to_string_lossy().replace('\\', "/");
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' {
        format!("/{}{}", (b[0] as char).to_ascii_lowercase(), &s[2..])
    } else {
        s
    }
}

/// Ruta corta para mostrar: `~\Desktop\Github`.
pub fn display(path: &Path) -> String {
    let p = normalize(path);
    let h = normalize(&home());
    let ps = p.to_string_lossy();
    let hs = h.to_string_lossy();
    if ps.len() >= hs.len() && ps[..hs.len()].eq_ignore_ascii_case(&hs) {
        let rest = &ps[hs.len()..];
        if rest.is_empty() || rest.starts_with('\\') {
            return format!("~{rest}");
        }
    }
    ps.into_owned()
}

/// Busca un ejecutable en el PATH (prueba .exe, .cmd y .bat).
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let has_ext = Path::new(name).extension().is_some();
    for dir in std::env::split_paths(&path) {
        if has_ext {
            let c = dir.join(name);
            if c.is_file() {
                return Some(c);
            }
            continue;
        }
        for ext in ["exe", "cmd", "bat"] {
            let c = dir.join(format!("{name}.{ext}"));
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

/// Ubicación de Git Bash (`bin\bash.exe`, que prepara el entorno MSYS).
pub fn find_git_bash(configured: Option<&str>) -> Option<PathBuf> {
    if let Some(c) = configured.filter(|c| !c.trim().is_empty()) {
        let p = expand(c);
        return p.is_file().then_some(p);
    }
    let mut candidates = vec![
        PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Git\bin\bash.exe"),
    ];
    if let Some(local) = dirs::data_local_dir() {
        candidates.push(local.join(r"Programs\Git\bin\bash.exe"));
    }
    // git.exe en Git\cmd\ → Git\bin\bash.exe
    if let Some(git) = which("git.exe")
        && let Some(root) = git.parent().and_then(Path::parent) {
            candidates.insert(0, root.join(r"bin\bash.exe"));
        }
    candidates.into_iter().find(|p| p.is_file())
}

/// `wt.exe` (alias de ejecución de la app de Windows Terminal).
pub fn find_wt() -> Option<PathBuf> {
    if let Some(local) = dirs::data_local_dir() {
        let alias = local.join(r"Microsoft\WindowsApps\wt.exe");
        // El alias es un reparse point: exists() puede fallar, symlink_metadata no.
        if std::fs::symlink_metadata(&alias).is_ok() {
            return Some(alias);
        }
    }
    which("wt.exe")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lowercases_drive_and_trims() {
        assert_eq!(normalize(Path::new(r"C:\Users\X\repo\")), PathBuf::from(r"c:\Users\X\repo"));
        assert_eq!(normalize(Path::new("C:/Users/X")), PathBuf::from(r"c:\Users\X"));
        assert_eq!(normalize(Path::new(r"\\?\C:\a")), PathBuf::from(r"c:\a"));
        assert_eq!(normalize(Path::new("/c/Users/X")), PathBuf::from(r"c:\Users\X"));
        assert_eq!(normalize(Path::new(r"C:\")), PathBuf::from(r"c:\"));
    }

    #[test]
    fn key_ignores_case() {
        assert_eq!(key(Path::new(r"C:\Users\X")), key(Path::new(r"c:\users\x\")));
    }

    #[test]
    fn msys_form() {
        assert_eq!(to_msys(Path::new(r"C:\Users\X Y\r")), "/c/Users/X Y/r");
    }
}
