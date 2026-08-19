//! Display helpers. Everything here is lossy-safe: paths are not guaranteed to
//! be UTF-8 on Linux and are UTF-16 on Windows.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use humansize::{format_size, BINARY};

/// "412.3 MiB"
pub fn bytes(n: u64) -> String {
    format_size(n, BINARY)
}

/// "12,481"
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "1m 04s"
pub fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 3600 {
        format!(
            "{}h {:02}m {:02}s",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    } else if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{}s", secs, d.subsec_millis() / 100)
    }
}

/// "12.4 MiB/s"
pub fn throughput(total_bytes: u64, elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 || total_bytes == 0 {
        return "—".to_string();
    }
    format!("{}/s", bytes((total_bytes as f64 / secs) as u64))
}

/// "2019-06-14 11:32" in UTC, or "—" when the timestamp is unavailable.
///
/// Formatted by hand from the Unix timestamp to avoid pulling in a date crate
/// for one label.
pub fn timestamp(t: Option<SystemTime>) -> String {
    let Some(t) = t else {
        return "—".to_string();
    };
    let Ok(dur) = t.duration_since(UNIX_EPOCH) else {
        return "—".to_string();
    };
    let secs = dur.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        tod / 3600,
        (tod % 3600) / 60
    )
}

/// Days since the Unix epoch to a calendar date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Render `path` relative to `root` when it lies inside it.
///
/// The scan root is already shown in the header, so repeating it on every row
/// wastes the columns that carry the actual difference between two copies.
pub fn relative_to(path: &Path, root: Option<&Path>) -> String {
    if let Some(root) = root {
        if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.to_string_lossy();
            if rel.is_empty() {
                return ".".to_string();
            }
            return rel.into_owned();
        }
    }
    path.to_string_lossy().into_owned()
}

/// Shorten a path to fit `width` columns, keeping the tail (the file name and
/// its immediate parents carry the information the user needs).
pub fn truncate_path(path: &str, width: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= width {
        return path.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let keep = width - 1;
    let tail: String = chars[chars.len() - keep..].iter().collect();
    format!("…{tail}")
}

/// Shorten any text to `width` columns, keeping the head.
pub fn truncate(text: &str, width: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let head: String = chars[..width - 1].iter().collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_get_thousands_separators() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000), "1,000");
        assert_eq!(count(12_481), "12,481");
        assert_eq!(count(1_234_567), "1,234,567");
    }

    #[test]
    fn civil_dates_match_known_values() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        // A leap day, to exercise the era arithmetic.
        assert_eq!(civil_from_days(19_784), (2024, 3, 2));
    }

    #[test]
    fn truncation_keeps_the_informative_end_of_a_path() {
        assert_eq!(truncate_path("/a/b/c.txt", 20), "/a/b/c.txt");
        // 16 chars trimmed to 8: an ellipsis plus the last 7 characters.
        assert_eq!(truncate_path("/aaaa/bbbb/c.txt", 8), "…b/c.txt");
    }

    #[test]
    fn paths_are_shown_relative_to_the_scan_root() {
        let root = Path::new("/home/me/pictures");
        assert_eq!(
            relative_to(Path::new("/home/me/pictures/2019/a.jpg"), Some(root)),
            "2019/a.jpg"
        );
        // The root itself.
        assert_eq!(relative_to(Path::new("/home/me/pictures"), Some(root)), ".");
        // Outside the root: fall back to the absolute path.
        assert_eq!(
            relative_to(Path::new("/mnt/other/a.jpg"), Some(root)),
            "/mnt/other/a.jpg"
        );
        // No root at all.
        assert_eq!(relative_to(Path::new("/a/b.jpg"), None), "/a/b.jpg");
    }

    #[test]
    fn truncation_never_exceeds_the_width() {
        for w in 1..12 {
            assert!(truncate_path("/some/long/path/file.bin", w).chars().count() <= w);
            assert!(truncate("a-long-label-here", w).chars().count() <= w);
        }
    }
}
