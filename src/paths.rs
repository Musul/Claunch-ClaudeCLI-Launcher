//! Rutas del sistema y normalización de rutas.
//!
//! Windows. Regla clave: Claude Code compara el `cwd` como texto. La extensión
//! de VS Code guarda `c:\...` (unidad en minúscula) y la CLI desde Git Bash
//! `C:\...`. claunch arranca siempre con la unidad en minúscula para que
//! `/resume` vea las sesiones de ambas superficies (ver
//! docs/diagnostico-resume-mayusculas.md). Las rutas se comparan sin distinguir
//! mayúsculas, como el sistema de archivos.
//!
//! Linux. Las rutas se usan tal cual (solo sin `/` final) y se comparan
//! distinguiendo mayúsculas.

use std::path::{Path, PathBuf};

/// Separador de rutas del sistema: `\` en Windows, `/` en Linux.
pub const SEP: char = std::path::MAIN_SEPARATOR;

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

/// Configuración y sets: `%APPDATA%\claunch` (Windows, roaming) o
/// `~/.config/claunch` (Linux, `XDG_CONFIG_HOME`).
pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(home).join("claunch")
}

/// Índice, log y archivos de arranque: `%LOCALAPPDATA%\claunch` (Windows) o
/// `~/.local/share/claunch` (Linux, `XDG_DATA_HOME`). `CLAUNCH_DATA_DIR`
/// permite otra carpeta (pruebas, como `CLAUNCH_CONFIG`).
pub fn data_dir() -> PathBuf {
    match std::env::var_os("CLAUNCH_DATA_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => dirs::data_local_dir().unwrap_or_else(home).join("claunch"),
    }
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
#[cfg(windows)]
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

/// Normaliza a la forma con la que claunch arranca Claude: sin `/` final.
#[cfg(not(windows))]
pub fn normalize(path: &Path) -> PathBuf {
    let mut s = path.to_string_lossy().into_owned();
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    PathBuf::from(s)
}

/// La ruta como la da Windows: igual que [`normalize`] pero con la unidad en
/// mayúscula (`C:\Users\x`). Es la grafía con la que arrancan los harness que
/// no son Claude. En Linux equivale a `normalize`.
pub fn natural(path: &Path) -> PathBuf {
    let p = normalize(path);
    if cfg!(windows) {
        let s = p.to_string_lossy();
        let b = s.as_bytes();
        if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_lowercase() {
            return PathBuf::from(format!("{}{}", (b[0] as char).to_ascii_uppercase(), &s[1..]));
        }
    }
    p
}

/// Clave para comparar rutas como lo hace el sistema de archivos: sin
/// distinguir mayúsculas en Windows, distinguiéndolas en Linux.
pub fn key(path: &Path) -> String {
    let s = normalize(path).to_string_lossy().into_owned();
    if cfg!(windows) { s.to_lowercase() } else { s }
}

/// `true` si la clave `k` es `root` o una ruta dentro de `root` (ambas de [`key`]).
pub fn key_within(k: &str, root: &str) -> bool {
    match k.strip_prefix(root) {
        Some("") => true,
        Some(rest) => rest.starts_with(SEP) || root.ends_with(SEP),
        None => false,
    }
}

/// `c:\Users\x` → `/c/Users/x` (para scripts de Git Bash).
#[cfg(windows)]
pub fn to_msys(path: &Path) -> String {
    let s = normalize(path).to_string_lossy().replace('\\', "/");
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' {
        format!("/{}{}", (b[0] as char).to_ascii_lowercase(), &s[2..])
    } else {
        s
    }
}

/// Ruta corta para mostrar: `~\Desktop\Github` (Windows) o `~/projects/app` (Linux).
pub fn display(path: &Path) -> String {
    let p = normalize(path);
    let h = normalize(&home());
    let ps = p.to_string_lossy();
    let hs = h.to_string_lossy();
    let same = |a: &str, b: &str| if cfg!(windows) { a.eq_ignore_ascii_case(b) } else { a == b };
    if ps.len() >= hs.len() && ps.is_char_boundary(hs.len()) && same(&ps[..hs.len()], &hs) {
        let rest = &ps[hs.len()..];
        if rest.is_empty() || rest.starts_with(SEP) {
            return format!("~{rest}");
        }
    }
    ps.into_owned()
}

/// Busca un ejecutable en el PATH. Las entradas vacías o relativas no
/// cuentan: la ruta que salga se usa después desde otra carpeta.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).filter(|dir| dir.is_absolute()).find_map(|dir| which_in(&dir, name))
}

/// El ejecutable `name` de una carpeta, si está (prueba .exe, .cmd y .bat).
#[cfg(windows)]
pub fn which_in(dir: &Path, name: &str) -> Option<PathBuf> {
    if Path::new(name).extension().is_some() {
        return Some(dir.join(name)).filter(|c| c.is_file());
    }
    ["exe", "cmd", "bat"].into_iter().map(|ext| dir.join(format!("{name}.{ext}"))).find(|c| c.is_file())
}

/// El ejecutable `name` de una carpeta, si está (archivo con permiso de ejecución).
#[cfg(not(windows))]
pub fn which_in(dir: &Path, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    Some(dir.join(name)).filter(|c| std::fs::metadata(c).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// Ubicación de Git Bash (`bin\bash.exe`, que prepara el entorno MSYS).
#[cfg(windows)]
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
#[cfg(windows)]
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

    #[cfg(windows)]
    #[test]
    fn normalize_lowercases_drive_and_trims() {
        assert_eq!(normalize(Path::new(r"C:\Users\X\repo\")), PathBuf::from(r"c:\Users\X\repo"));
        assert_eq!(normalize(Path::new("C:/Users/X")), PathBuf::from(r"c:\Users\X"));
        assert_eq!(normalize(Path::new(r"\\?\C:\a")), PathBuf::from(r"c:\a"));
        assert_eq!(normalize(Path::new("/c/Users/X")), PathBuf::from(r"c:\Users\X"));
        assert_eq!(normalize(Path::new(r"C:\")), PathBuf::from(r"c:\"));
    }

    #[cfg(windows)]
    #[test]
    fn natural_uppercases_drive() {
        assert_eq!(natural(Path::new(r"c:\Users\X\repo\")), PathBuf::from(r"C:\Users\X\repo"));
        assert_eq!(natural(Path::new("c:/Users/X")), PathBuf::from(r"C:\Users\X"));
        assert_eq!(natural(Path::new("C:/Users/X/")), PathBuf::from(r"C:\Users\X"));
        assert_eq!(natural(Path::new(r"c:\")), PathBuf::from(r"C:\"));
        assert_eq!(natural(Path::new(r"\\servidor\recurso")), PathBuf::from(r"\\servidor\recurso"));
        // Misma carpeta para el sistema de archivos, se escriba como se escriba.
        assert_eq!(key(&natural(Path::new(r"c:\r\app"))), key(Path::new("C:/r/app/")));
    }

    #[cfg(not(windows))]
    #[test]
    fn natural_keeps_unix_paths() {
        assert_eq!(natural(Path::new("/home/ana/Repo/")), PathBuf::from("/home/ana/Repo"));
        assert_eq!(natural(Path::new("/")), PathBuf::from("/"));
    }

    #[cfg(windows)]
    #[test]
    fn key_ignores_case() {
        assert_eq!(key(Path::new(r"C:\Users\X")), key(Path::new(r"c:\users\x\")));
        assert!(key_within(&key(Path::new(r"C:\R\App\sub")), &key(Path::new(r"c:\r\app"))));
        assert!(!key_within(&key(Path::new(r"c:\r\app2")), &key(Path::new(r"c:\r\app"))));
        assert!(key_within(&key(Path::new(r"c:\r")), &key(Path::new(r"c:\"))));
    }

    #[cfg(windows)]
    #[test]
    fn msys_form() {
        assert_eq!(to_msys(Path::new(r"C:\Users\X Y\r")), "/c/Users/X Y/r");
    }

    #[cfg(not(windows))]
    #[test]
    fn normalize_keeps_unix_paths() {
        assert_eq!(normalize(Path::new("/home/ana/repo/")), PathBuf::from("/home/ana/repo"));
        assert_eq!(normalize(Path::new("/home/ana/Repo")), PathBuf::from("/home/ana/Repo"));
        assert_eq!(normalize(Path::new("/")), PathBuf::from("/"));
    }

    #[cfg(not(windows))]
    #[test]
    fn key_is_case_sensitive() {
        assert_ne!(key(Path::new("/home/ana/Repo")), key(Path::new("/home/ana/repo")));
        assert_eq!(key(Path::new("/home/ana/repo/")), key(Path::new("/home/ana/repo")));
        assert!(key_within("/home/ana/repo/sub", "/home/ana/repo"));
        assert!(key_within("/home/ana/repo", "/home/ana/repo"));
        assert!(!key_within("/home/ana/repo2", "/home/ana/repo"));
        assert!(key_within("/home", "/"));
    }

    #[test]
    fn display_shortens_home() {
        let inside = home().join("proyectos").join("app");
        assert_eq!(display(&inside), format!("~{SEP}proyectos{SEP}app"));
        assert_eq!(display(&home()), "~");
    }

    #[cfg(not(windows))]
    #[test]
    fn which_needs_exec_bit() {
        assert!(which("sh").is_some());
        assert!(which("claunch-no-existe-xyz").is_none());
    }
}
