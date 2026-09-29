//! Text formatting helpers: tables, sizes and times.

use std::time::{SystemTime, UNIX_EPOCH};

/// Makes a table with aligned columns. The last column has no padding.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let columns = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(columns) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let mut out = String::new();
    let mut push_row = |cells: Vec<&str>| {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 < cells.len() {
                line.push_str(cell);
                let pad = widths[i] - cell.chars().count();
                line.push_str(&" ".repeat(pad + 2));
            } else {
                line.push_str(cell);
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    };
    push_row(headers.to_vec());
    for row in rows {
        push_row(row.iter().map(String::as_str).collect());
    }
    out
}

/// Formats a size in bytes, for example `512 B`, `1.5 KiB`, `3.0 MiB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Formats the time of day in UTC, for example `14:05:09Z`.
pub fn time_of_day(time: SystemTime) -> String {
    let secs = time
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = secs % 86_400;
    format!(
        "{:02}:{:02}:{:02}Z",
        day / 3600,
        (day % 3600) / 60,
        day % 60
    )
}

/// Returns the value, or `-` when it is empty.
pub fn or_dash(value: Option<&str>) -> String {
    match value {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => "-".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn table_aligns_columns() {
        let out = table(
            &["ID", "NAME", "X"],
            &[
                vec!["1".into(), "long name".into(), "a".into()],
                vec!["22".into(), "n".into(), "".into()],
            ],
        );
        assert_eq!(out, "ID  NAME       X\n1   long name  a\n22  n\n");
    }

    #[test]
    fn table_without_rows_has_header() {
        assert_eq!(table(&["A", "B"], &[]), "A  B\n");
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MiB");
        assert_eq!(human_size(5 * 1024 * 1024 * 1024), "5.0 GiB");
    }

    #[test]
    fn time_of_day_is_utc() {
        let t = UNIX_EPOCH + Duration::from_secs(86_400 * 3 + 3600 * 14 + 60 * 5 + 9);
        assert_eq!(time_of_day(t), "14:05:09Z");
    }

    #[test]
    fn dash_for_empty_values() {
        assert_eq!(or_dash(None), "-");
        assert_eq!(or_dash(Some("")), "-");
        assert_eq!(or_dash(Some("x")), "x");
    }
}
