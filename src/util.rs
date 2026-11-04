//! Utilidades: escritura atómica, texto y formato de fechas/tamaños en español.

use std::path::Path;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Escribe a un temporal y lo renombra encima (reemplaza en Windows).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

/// Minúsculas y sin tildes, para filtrar ("Gestión" ~ "gestion").
pub fn fold(s: &str) -> String {
    s.chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'á' | 'à' | 'ä' | 'â' => 'a',
            'é' | 'è' | 'ë' | 'ê' => 'e',
            'í' | 'ì' | 'ï' | 'î' => 'i',
            'ó' | 'ò' | 'ö' | 'ô' => 'o',
            'ú' | 'ù' | 'ü' | 'û' => 'u',
            'ñ' => 'n',
            c => c,
        })
        .collect()
}

/// Todas las palabras del filtro aparecen en el texto.
pub fn matches(filter: &str, haystack: &str) -> bool {
    let h = fold(haystack);
    fold(filter).split_whitespace().all(|w| h.contains(w))
}

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Recorta a `max` columnas con `…`.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Recorta por la izquierda (para rutas): `…\Github\repo`.
pub fn truncate_left(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut w = 0;
    let mut start = chars.len();
    while start > 0 {
        let cw = chars[start - 1].width().unwrap_or(0);
        if w + cw > max - 1 {
            break;
        }
        w += cw;
        start -= 1;
    }
    format!("…{}", chars[start..].iter().collect::<String>())
}

const WEEKDAYS: [&str; 7] = ["lun", "mar", "mié", "jue", "vie", "sáb", "dom"];
const MONTHS: [&str; 12] = ["ene", "feb", "mar", "abr", "may", "jun", "jul", "ago", "sep", "oct", "nov", "dic"];

/// "ahora", "hace 5 min", "hace 3 h", "ayer 17:02", "lun 09:15", "28 oct", "3 mar 2025".
pub fn relative_time(ms: i64, now_ms: i64) -> String {
    if ms <= 0 {
        return "—".into();
    }
    let diff = (now_ms - ms).max(0) / 1000;
    if diff < 45 {
        return "ahora".into();
    }
    if diff < 3600 {
        return format!("hace {} min", (diff + 30) / 60);
    }
    if diff < 6 * 3600 {
        return format!("hace {} h", diff / 3600);
    }
    let tz = jiff::tz::TimeZone::system();
    let (Ok(t), Ok(n)) = (jiff::Timestamp::from_millisecond(ms), jiff::Timestamp::from_millisecond(now_ms)) else {
        return "—".into();
    };
    let t = t.to_zoned(tz.clone());
    let n = n.to_zoned(tz);
    let hm = format!("{:02}:{:02}", t.hour(), t.minute());
    let days = t.date().until(n.date()).map(|s| s.get_days()).unwrap_or(0);
    match days {
        0 => format!("hoy {hm}"),
        1 => format!("ayer {hm}"),
        2..=6 => format!("{} {hm}", WEEKDAYS[t.weekday().to_monday_zero_offset() as usize]),
        _ if t.year() == n.year() => format!("{} {}", t.day(), MONTHS[(t.month() - 1) as usize]),
        _ => format!("{} {} {}", t.day(), MONTHS[(t.month() - 1) as usize], t.year()),
    }
}

/// "824 KB", "7,3 MB".
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{:.1} MB", b / KB / KB).replace('.', ",")
    }
}

pub fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_and_matching() {
        assert!(matches("gestion tien", "Gestión de la Tienda"));
        assert!(!matches("vm", "tienda"));
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(truncate_left(r"c:\a\b\repo", 6), "…\\repo");
    }

    #[test]
    fn relative() {
        let now = 1_793_800_000_000;
        assert_eq!(relative_time(now - 10_000, now), "ahora");
        assert_eq!(relative_time(now - 5 * 60_000, now), "hace 5 min");
        assert_eq!(relative_time(now - 2 * 3_600_000, now), "hace 2 h");
        assert!(relative_time(now - 30 * 86_400_000, now).contains(' '));
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(500), "500 B");
        assert_eq!(human_size(7_655_000), "7,3 MB");
    }
}
