//! Descubrimiento de repos: escaneo de carpetas raíz + raíces deducidas del
//! `cwd` de cada sesión (subcarpetas y worktrees se agrupan bajo su repo).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::paths;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootKind {
    Git,
    /// Carpeta sin git con sesiones de Claude.
    Folder,
}

/// Dónde cae un `cwd` de sesión.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub root: PathBuf,
    pub kind: RootKind,
    /// Subcarpeta relativa a la raíz (`apps\harness`), si la sesión no está en la raíz.
    pub subpath: Option<String>,
    /// Nombre del worktree enlazado, si la sesión vive en uno.
    pub worktree: Option<String>,
}

const MARKER: &str = ".git";

/// Escanea `roots` hasta `depth` niveles buscando carpetas con `.git`.
/// Sigue bajando dentro de los repos (hay repos anidados), sin entrar en
/// carpetas pesadas ni en puntos de reanálisis (junctions/symlinks).
pub fn scan_roots(roots: &[PathBuf], depth: usize, exclude: &[String]) -> Vec<PathBuf> {
    let exclude: Vec<String> = exclude.iter().map(|e| e.to_lowercase()).collect();
    let mut found: Vec<PathBuf> = roots
        .par_iter()
        .filter(|r| r.is_dir())
        .flat_map_iter(|r| {
            let mut out = Vec::new();
            walk(r, depth, &exclude, &mut out);
            out
        })
        .collect();
    found.sort_by_key(|p| paths::key(p));
    found.dedup_by_key(|p| paths::key(p));
    found
}

fn is_reparse_point(md: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        md.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        md.file_type().is_symlink()
    }
}

fn walk(dir: &Path, depth: usize, exclude: &[String], out: &mut Vec<PathBuf>) {
    let git = dir.join(MARKER);
    if let Ok(md) = std::fs::symlink_metadata(&git) {
        // `.git` archivo = worktree enlazado o submódulo; los worktrees se
        // agrupan bajo su repo principal al ubicar sesiones.
        if md.is_dir() || (md.is_file() && linked_worktree(&git).is_none()) {
            out.push(paths::normalize(dir));
        }
    }
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name.starts_with('$') || exclude.contains(&name.to_lowercase()) {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        if !md.is_dir() || is_reparse_point(&md) {
            continue;
        }
        walk(&e.path(), depth - 1, exclude, out);
    }
}

/// Si `.git` es un archivo de worktree enlazado, devuelve (repo principal, nombre).
fn linked_worktree(git_file: &Path) -> Option<(PathBuf, String)> {
    let text = std::fs::read_to_string(git_file).ok()?;
    let gitdir = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let mut gitdir = PathBuf::from(gitdir);
    if gitdir.is_relative() {
        gitdir = git_file.parent()?.join(gitdir);
    }
    let norm = paths::normalize(&gitdir);
    let s = norm.to_string_lossy();
    let lower = s.to_lowercase();
    let idx = lower.rfind(r"\.git\worktrees\")?;
    let main = PathBuf::from(&s[..idx]);
    let name = s[idx + r"\.git\worktrees\".len()..].to_string();
    Some((main, name))
}

/// Ubica cada `cwd` en su repo. `known` son las raíces escaneadas (sirven de
/// respaldo para carpetas que ya no existen).
pub fn place_all(cwds: &[String], known: &[PathBuf]) -> HashMap<String, Placement> {
    let mut known_sorted: Vec<(String, PathBuf)> = known.iter().map(|k| (paths::key(k), k.clone())).collect();
    // Más largas primero: el repo más profundo gana (repos anidados).
    known_sorted.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));
    cwds.par_iter()
        .map(|cwd| (paths::key(Path::new(cwd)), place(cwd, &known_sorted)))
        .collect::<Vec<_>>()
        .into_iter()
        .collect()
}

fn place(cwd: &str, known: &[(String, PathBuf)]) -> Placement {
    let cwd_path = paths::normalize(Path::new(cwd));
    if cwd_path.is_dir() {
        let mut cur: Option<&Path> = Some(&cwd_path);
        while let Some(dir) = cur {
            let git = dir.join(MARKER);
            if let Ok(md) = std::fs::symlink_metadata(&git) {
                if md.is_file()
                    && let Some((main, name)) = linked_worktree(&git) {
                        let main = paths::normalize(&main);
                        return Placement {
                            subpath: rel(&cwd_path, dir),
                            root: main,
                            kind: RootKind::Git,
                            worktree: Some(name),
                        };
                    }
                return Placement {
                    root: dir.to_path_buf(),
                    kind: RootKind::Git,
                    subpath: rel(&cwd_path, dir),
                    worktree: None,
                };
            }
            cur = dir.parent();
        }
        return Placement { root: cwd_path, kind: RootKind::Folder, subpath: None, worktree: None };
    }
    // La carpeta ya no existe: se asocia al repo conocido más profundo que la contenga.
    let key = paths::key(&cwd_path);
    for (k, root) in known {
        if key == *k || key.starts_with(&format!("{k}\\")) {
            return Placement { root: root.clone(), kind: RootKind::Git, subpath: rel(&cwd_path, root), worktree: None };
        }
    }
    Placement { root: cwd_path, kind: RootKind::Folder, subpath: None, worktree: None }
}

fn rel(path: &Path, root: &Path) -> Option<String> {
    let p = paths::key(path);
    let r = paths::key(root);
    if p == r {
        return None;
    }
    let full = paths::normalize(path);
    let s = full.to_string_lossy();
    (p.starts_with(&r) && s.len() > r.len() + 1).then(|| s[r.len() + 1..].to_string())
}

/// Última actividad git (ms): el más reciente entre HEAD, index, logs/HEAD y FETCH_HEAD.
pub fn git_activity_ms(root: &Path) -> Option<i64> {
    let git = root.join(MARKER);
    let gitdir = if git.is_dir() {
        git
    } else {
        let text = std::fs::read_to_string(&git).ok()?;
        let d = PathBuf::from(text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim());
        if d.is_relative() { root.join(d) } else { d }
    };
    ["HEAD", "index", r"logs\HEAD", "FETCH_HEAD"]
        .iter()
        .filter_map(|f| std::fs::metadata(gitdir.join(f)).ok()?.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_subfolder_and_missing() {
        let base = std::env::temp_dir().join("claunch-tests").join("repos");
        let repo = base.join("r1");
        let sub = repo.join("apps").join("harness");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let known = scan_roots(std::slice::from_ref(&base), 3, &[]);
        assert!(known.iter().any(|k| paths::key(k) == paths::key(&repo)));

        let sub_s = sub.to_string_lossy().to_uppercase();
        let missing = repo.join("borrada").to_string_lossy().into_owned();
        let placed = place_all(&[sub_s.clone(), missing.clone()], &known);
        let a = &placed[&paths::key(Path::new(&sub_s))];
        assert_eq!(paths::key(&a.root), paths::key(&repo));
        assert_eq!(a.subpath.as_deref().map(str::to_lowercase), Some(r"apps\harness".to_string()));
        let b = &placed[&paths::key(Path::new(&missing))];
        assert_eq!(paths::key(&b.root), paths::key(&repo));
        assert_eq!(b.kind, RootKind::Git);
    }

    #[test]
    fn linked_worktree_groups_under_main() {
        let base = std::env::temp_dir().join("claunch-tests").join("wt");
        let main = base.join("main");
        let wt = base.join("wt1");
        std::fs::create_dir_all(main.join(".git").join("worktrees").join("wt1")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        let gitdir = main.join(".git").join("worktrees").join("wt1");
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        let p = place(&wt.to_string_lossy(), &[]);
        assert_eq!(paths::key(&p.root), paths::key(&main));
        assert_eq!(p.worktree.as_deref(), Some("wt1"));
        // El escaneo no lista el worktree como repo aparte.
        let known = scan_roots(std::slice::from_ref(&base), 2, &[]);
        assert!(!known.iter().any(|k| paths::key(k) == paths::key(&wt)));
    }
}
