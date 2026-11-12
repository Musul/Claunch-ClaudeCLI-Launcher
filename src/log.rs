//! Bitácora local en `claunch.log`, en la carpeta de datos de claunch
//! (`%LOCALAPPDATA%\claunch` en Windows, `~/.local/share/claunch` en Linux).
//! Todo fallo degradado (archivo ilegible, proceso no verificable, escaneo
//! parcial) queda aquí; los fallos que el usuario debe ver van además a un toast.

use std::io::Write;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());
const MAX_BYTES: u64 = 1024 * 1024;

pub fn write(level: &str, msg: &str) {
    let _guard = LOCK.lock();
    let dir = crate::paths::data_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("claunch.log");
    if std::fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join("claunch.log.1"));
    }
    let now = jiff::Timestamp::now();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{now} {level} {msg}");
    }
}

#[macro_export]
macro_rules! warn_log {
    ($($arg:tt)*) => { $crate::log::write("WARN", &format!($($arg)*)) };
}

#[macro_export]
macro_rules! info_log {
    ($($arg:tt)*) => { $crate::log::write("INFO", &format!($($arg)*)) };
}
