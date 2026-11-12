//! `claunch setup`: instala el ejecutable en `~/.local/bin`. En Windows añade
//! además perfiles a Windows Terminal mediante un fragmento JSON (no toca su
//! settings.json); `claunch setup --uninstall` borra el fragmento.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;

use crate::paths;

/// GUIDs fijos para que reinstalar actualice los mismos perfiles.
#[cfg(windows)]
pub const GIT_BASH_PROFILE: &str = "{3c7d5a1e-8f42-4b6c-9d13-0a5e7c2b9f41}";
#[cfg(windows)]
const LAUNCHER_PROFILE: &str = "{9b2e4f6a-1c3d-4e5f-8a7b-6c5d4e3f2a10}";
#[cfg(windows)]
const SCHEME: &str = "Claude Warm";

const EXE: &str = if cfg!(windows) { "claunch.exe" } else { "claunch" };

#[cfg(windows)]
fn fragment_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(paths::home).join(r"Microsoft\Windows Terminal\Fragments\claunch")
}

fn install_dir() -> PathBuf {
    paths::home().join(".local").join("bin")
}

pub fn main(args: &[String]) -> ExitCode {
    let result = if args.iter().any(|a| a == "--uninstall") { uninstall() } else { install() };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("claunch setup: {e:#}");
            ExitCode::from(1)
        }
    }
}

#[cfg(windows)]
fn uninstall() -> anyhow::Result<()> {
    let dir = fragment_dir();
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("no se pudo borrar {}", dir.display()))?;
        println!("✓ fragmento de Windows Terminal borrado: {}", dir.display());
    } else {
        println!("· no había fragmento en {}", dir.display());
    }
    println!("  El ejecutable en {} se conserva; bórralo a mano si quieres.", install_dir().display());
    Ok(())
}

#[cfg(not(windows))]
fn uninstall() -> anyhow::Result<()> {
    println!("· en Linux claunch no crea perfiles: no hay nada más que borrar.");
    println!("  El ejecutable {} se conserva; bórralo a mano si quieres.", install_dir().join(EXE).display());
    Ok(())
}

fn install() -> anyhow::Result<()> {
    let dst = install_binary()?;
    #[cfg(windows)]
    wt_profiles(&dst)?;
    #[cfg(not(windows))]
    {
        let _ = dst;
        println!("\nListo: escribe «claunch» en cualquier terminal.");
    }
    Ok(())
}

/// Copia el ejecutable actual a `~/.local/bin` (si no se ejecuta ya desde ahí).
fn install_binary() -> anyhow::Result<PathBuf> {
    let src = std::env::current_exe()?;
    let dir = install_dir();
    let dst = dir.join(EXE);
    std::fs::create_dir_all(&dir)?;
    if paths::key(&src) != paths::key(&dst) {
        replace_binary(&src, &dst)?;
        let a = std::fs::metadata(&src)?.len();
        let b = std::fs::metadata(&dst)?.len();
        anyhow::ensure!(a == b, "la copia no coincide en tamaño ({a} vs {b} bytes)");
        println!("✓ instalado {} ({} KB)", dst.display(), b / 1024);
    } else {
        println!("· ya se ejecuta desde {}", dst.display());
    }
    if paths::which(EXE).is_none_or(|p| paths::key(&p) != paths::key(&dst)) {
        println!("  ⚠ {} no está primero en el PATH", dir.display());
        #[cfg(not(windows))]
        println!("    Añade a tu ~/.bashrc o ~/.zshrc:  export PATH=\"$HOME/.local/bin:$PATH\"");
    }
    Ok(dst)
}

/// Un .exe en uso no se puede sobrescribir, pero sí renombrar.
#[cfg(windows)]
fn replace_binary(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    if dst.exists() {
        let old = dst.with_extension("old.exe");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(dst, &old).with_context(|| format!("no se pudo apartar {}", dst.display()))?;
    }
    std::fs::copy(src, dst).with_context(|| format!("no se pudo copiar a {}", dst.display()))?;
    Ok(())
}

/// Copia a un temporal y lo renombra encima: un binario en ejecución no se
/// puede sobrescribir («Text file busy»), pero sí reemplazar.
#[cfg(not(windows))]
fn replace_binary(src: &std::path::Path, dst: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = dst.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::copy(src, &tmp).with_context(|| format!("no se pudo copiar a {}", tmp.display()))?;
    let result = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
        .and_then(|()| std::fs::rename(&tmp, dst))
        .with_context(|| format!("no se pudo instalar {}", dst.display()));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Perfiles «claunch» y «Claude · Git Bash» de Windows Terminal.
#[cfg(windows)]
fn wt_profiles(dst: &std::path::Path) -> anyhow::Result<()> {
    use serde_json::json;

    use crate::config::Config;

    let bash = paths::find_git_bash(None).context("no encuentro Git Bash")?;
    let git_root = bash.parent().and_then(|p| p.parent()).map(PathBuf::from).unwrap_or_default();
    let icon = git_root.join(r"mingw64\share\git\git-for-windows.ico");
    let mut git_profile = json!({
        "guid": GIT_BASH_PROFILE,
        "name": "Claude · Git Bash",
        "commandline": format!("\"{}\" --login -i", bash.display()),
        "startingDirectory": "%USERPROFILE%",
        "colorScheme": SCHEME,
    });
    if icon.is_file() {
        git_profile["icon"] = json!(icon.to_string_lossy());
    }
    let fragment = json!({
        "profiles": [
            git_profile,
            {
                "guid": LAUNCHER_PROFILE,
                "name": "claunch",
                "commandline": format!("\"{}\"", dst.display()),
                "icon": "✳",
                "startingDirectory": "%USERPROFILE%",
                "colorScheme": SCHEME,
                "tabColor": "#D97757",
            }
        ],
        "schemes": [{
            "name": SCHEME,
            "background": "#171613", "foreground": "#EEEAE0", "cursorColor": "#D97757", "selectionBackground": "#3A261E",
            "black": "#171613", "red": "#E8655A", "green": "#93C480", "yellow": "#E8B94F",
            "blue": "#7AAAE0", "purple": "#B997E0", "cyan": "#7FC4C0", "white": "#EEEAE0",
            "brightBlack": "#6E695F", "brightRed": "#F08A80", "brightGreen": "#ABD69A", "brightYellow": "#F2CE7A",
            "brightBlue": "#9CC2EC", "brightPurple": "#CDB3EC", "brightCyan": "#A0D8D4", "brightWhite": "#FFFFFF"
        }]
    });
    let path = fragment_dir().join("claunch.json");
    crate::util::write_atomic(&path, serde_json::to_string_pretty(&fragment)?.as_bytes())?;
    println!("✓ perfiles de Windows Terminal: {}", path.display());

    // Que las pestañas hereden icono y colores del perfil nuevo.
    let (mut cfg, err) = Config::load();
    match err {
        Some(e) => println!("  ⚠ no toco config.toml: {e}"),
        None if cfg.general.wt_profile.is_empty() => {
            cfg.general.wt_profile = GIT_BASH_PROFILE.into();
            cfg.save()?;
            println!("✓ config.toml: general.wt_profile = perfil «Claude · Git Bash»");
        }
        None => println!("· config.toml ya usa el perfil {}", cfg.general.wt_profile),
    }
    println!("\nReinicia Windows Terminal para ver los perfiles «claunch» y «Claude · Git Bash».");
    Ok(())
}
