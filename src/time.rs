use anyhow::{Result, bail};
use chrono::{Local, NaiveDate, TimeZone};

pub fn fmt_ts(ts: i64) -> String {
    match Local.timestamp_opt(ts, 0).single() {
        Some(d) => d.format("%Y-%m-%d %H:%M").to_string(),
        None => String::new(),
    }
}

pub fn now() -> i64 {
    Local::now().timestamp()
}

/// Accepts `2026-11-03`, `2026-11-03 14:00`, RFC 3339, or relative spans like
/// `30m`, `12h`, `7d`, `2w` (meaning "that long ago").
pub fn parse_when(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Some((num, unit)) = s
        .char_indices()
        .last()
        .map(|(i, c)| (&s[..i], c))
        .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    {
        let n: i64 = num.parse()?;
        let secs = match unit {
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'w' => 604_800,
            'y' => 31_536_000,
            _ => bail!("unknown time unit in '{s}' (use m, h, d, w or y)"),
        };
        return Ok(now() - n * secs);
    }
    match s.to_lowercase().as_str() {
        "today" | "heute" => {
            let d = Local::now().date_naive();
            return local_midnight(d);
        }
        "yesterday" | "gestern" => {
            let d = Local::now().date_naive() - chrono::Duration::days(1);
            return local_midnight(d);
        }
        _ => {}
    }
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(d.timestamp());
    }
    for f in ["%Y-%m-%d %H:%M", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S"] {
        if let Ok(d) = chrono::NaiveDateTime::parse_from_str(s, f)
            && let Some(t) = Local.from_local_datetime(&d).single() {
                return Ok(t.timestamp());
            }
    }
    for f in ["%Y-%m-%d", "%d.%m.%Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, f) {
            return local_midnight(d);
        }
    }
    bail!("cannot read date '{s}'. Use 2026-11-03, 2026-11-03 14:00, or 7d / 12h")
}

fn local_midnight(d: NaiveDate) -> Result<i64> {
    let dt = d.and_hms_opt(0, 0, 0).unwrap();
    match Local.from_local_datetime(&dt).earliest() {
        Some(t) => Ok(t.timestamp()),
        None => bail!("invalid local date"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_absolute() {
        let n = now();
        assert!((parse_when("7d").unwrap() - (n - 7 * 86_400)).abs() < 5);
        assert!(parse_when("2026-11-03").is_ok());
        assert!(parse_when("2026-11-03 14:00").is_ok());
        assert!(parse_when("03.11.2026").is_ok());
        assert!(parse_when("7x").is_err());
        assert!(parse_when("soon").is_err());
    }
}
