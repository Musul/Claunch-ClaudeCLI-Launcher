//! Lectura de las bases SQLite de otros programas (opencode, Antigravity).
//!
//! claunch es un invitado: abre en solo lectura, no hace checkpoint ni cambia
//! el modo de diario, y espera poco si el dueño está escribiendo. Las consultas
//! son cortas para no retener el WAL.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};

/// Abre una base de datos ajena en solo lectura.
pub fn open_readonly(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(Duration::from_millis(2000))?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

/// `true` si la tabla (o vista) existe: los esquemas cambian entre versiones.
pub fn has_table(conn: &Connection, name: &str) -> bool {
    conn.query_row("SELECT 1 FROM sqlite_master WHERE type IN ('table', 'view') AND name = ?1", [name], |_| Ok(())).is_ok()
}

/// Columnas de una tabla (vacío si no existe).
pub fn columns(conn: &Connection, table: &str) -> Vec<String> {
    let Ok(mut stmt) = conn.prepare("SELECT name FROM pragma_table_info(?1)") else { return Vec::new() };
    let Ok(rows) = stmt.query_map([table], |r| r.get::<_, String>(0)) else { return Vec::new() };
    rows.flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_readonly_and_inspects_schema() {
        let dir = std::env::temp_dir().join("claunch-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("sqlite-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let rw = Connection::open(&path).unwrap();
            rw.execute_batch("CREATE TABLE sesion (id TEXT PRIMARY KEY, titulo TEXT); INSERT INTO sesion VALUES ('a', 'Hola');").unwrap();
        }
        let conn = open_readonly(&path).unwrap();
        assert!(has_table(&conn, "sesion"));
        assert!(!has_table(&conn, "no_existe"));
        assert_eq!(columns(&conn, "sesion"), ["id", "titulo"]);
        assert!(columns(&conn, "no_existe").is_empty());
        let titulo: String = conn.query_row("SELECT titulo FROM sesion WHERE id = 'a'", [], |r| r.get(0)).unwrap();
        assert_eq!(titulo, "Hola");
        // Solo lectura: escribir falla y no toca el archivo.
        assert!(conn.execute("INSERT INTO sesion VALUES ('b', 'x')", []).is_err());
        assert!(open_readonly(&dir.join("no-existe.db")).is_err());
        drop(conn);
        let _ = std::fs::remove_file(&path);
    }
}
