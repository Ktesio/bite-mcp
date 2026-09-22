//! Date parsing: strict ISO 8601 for MCP clients, plus a pragmatic relaxed
//! parser for humans on the CLI ("tomorrow 3pm", "in 2h", "fri 09:30").

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, Timelike};

/// Parse a date(-time) string into local time, rendered back as RFC 3339 with
/// the local offset (the protocol's canonical form).
pub fn parse_to_rfc3339(s: &str) -> Result<String, String> {
    let dt = parse(s)?;
    Ok(dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
}

pub fn parse(s: &str) -> Result<DateTime<Local>, String> {
    let s = s.trim();
    let lower = s.to_ascii_lowercase();

    // strict ISO with offset / Z
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Local));
    }
    // "YYYY-MM-DD HH:MM[:SS]"
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(ndt
            .and_local_timezone(Local)
            .single()
            .ok_or("ambiguous time")?);
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return Ok(ndt
            .and_local_timezone(Local)
            .single()
            .ok_or("ambiguous time")?);
    }
    // "YYYY-MM-DD"
    if let Ok(nd) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(nd
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .single()
            .ok_or("ambiguous date")?);
    }

    // ── relaxed forms ──
    // "in <N> (min|m|h|hour|d|day|week)s?"
    if let Some(rest) = lower.strip_prefix("in ") {
        let mut it = rest.split_whitespace();
        // accept both "in 2h" and "in 2 hours"
        let tok = it.next().unwrap_or("");
        let digits: String = tok.chars().take_while(|c| c.is_ascii_digit()).collect();
        let unit_part: String = tok.chars().skip_while(|c| c.is_ascii_digit()).collect();
        let n: i64 = digits.parse().map_err(|_| format!("cannot parse '{s}'"))?;
        let unit = if unit_part.is_empty() {
            it.next().unwrap_or("h").trim_end_matches('s').to_string()
        } else {
            unit_part
                .trim_end_matches('s')
                .trim_end_matches('.')
                .to_string()
        };
        let now = Local::now();
        let dt = match unit.as_str() {
            "min" | "m" => now + Duration::minutes(n),
            "h" | "hour" => now + Duration::hours(n),
            "d" | "day" => now + Duration::days(n),
            "w" | "week" => now + Duration::weeks(n),
            _ => return Err(format!("unknown unit in '{s}'")),
        };
        return Ok(truncate_minutes(dt));
    }

    // "[today|tonight|tomorrow|<weekday>] [HH:MM[:AM|PM]]"
    let mut words = lower.split_whitespace();
    let first = words.next().unwrap_or("");
    let time_str = words.next().unwrap_or("");
    let base = match first {
        "today" | "tonight" => Some(Local::now().date_naive()),
        "tomorrow" => Some((Local::now() + Duration::days(1)).date_naive()),
        "yesterday" => Some((Local::now() - Duration::days(1)).date_naive()),
        w if is_weekday(w) => {
            let target = weekday_index(w);
            let today_idx = Local::now().weekday().num_days_from_monday() as i64;
            let mut delta = (target - today_idx).rem_euclid(7);
            if delta == 0 {
                delta = 7; // "monday" means next monday
            }
            Some((Local::now() + Duration::days(delta)).date_naive())
        }
        _ => None,
    };
    if let Some(date) = base {
        let (h, m) = parse_clock(time_str).unwrap_or((9, 0));
        let ndt = date.and_hms_opt(h, m, 0).ok_or("invalid time")?;
        return Ok(ndt
            .and_local_timezone(Local)
            .single()
            .ok_or("ambiguous time")?);
    }

    Err(format!(
        "cannot parse date '{s}' — use ISO 8601 (2026-09-21T15:00:00+02:00) or relaxed forms like 'tomorrow 3pm', 'in 2h', 'fri 09:30'"
    ))
}

fn truncate_minutes(dt: DateTime<Local>) -> DateTime<Local> {
    dt.with_second(0)
        .unwrap_or(dt)
        .with_nanosecond(0)
        .unwrap_or(dt)
}

fn is_weekday(w: &str) -> bool {
    ["mon", "tue", "wed", "thu", "fri", "sat", "sun"]
        .iter()
        .any(|d| w.starts_with(d))
}

fn weekday_index(w: &str) -> i64 {
    match w {
        w if w.starts_with("mon") => 0,
        w if w.starts_with("tue") => 1,
        w if w.starts_with("wed") => 2,
        w if w.starts_with("thu") => 3,
        w if w.starts_with("fri") => 4,
        w if w.starts_with("sat") => 5,
        _ => 6,
    }
}

/// "3pm", "15:00", "9.30am", "0900"
fn parse_clock(s: &str) -> Option<(u32, u32)> {
    let s = s.trim();
    if s.is_empty() {
        return Some((9, 0));
    }
    let (body, pm) = if let Some(b) = s.strip_suffix("pm") {
        (b.trim(), true)
    } else if let Some(b) = s.strip_suffix("am") {
        (b.trim(), false)
    } else {
        (s, false)
    };
    let (h, m) = if let Some((h, m)) = body.split_once(':') {
        (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?)
    } else if let Some((h, m)) = body.split_once('.') {
        (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?)
    } else if body.len() == 4 && body.chars().all(|c| c.is_ascii_digit()) {
        (body[..2].parse().ok()?, body[2..].parse().ok()?)
    } else {
        (body.parse::<u32>().ok()?, 0)
    };
    let mut h = h;
    if pm {
        if h < 12 {
            h += 12;
        }
    } else if h == 12 {
        h = 0; // 12am is midnight
    }
    if h > 23 || m > 59 {
        return None;
    }
    Some((h, m))
}

/// RFC3339/relaxed date string → epoch milliseconds (dates parse as local
/// midnight). Used by the index layer for range filters.
pub fn parse_to_epoch_ms(s: &str) -> Option<i64> {
    parse(s).ok().map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_with_offset() {
        assert!(parse("2026-09-21T15:00:00+02:00").is_ok());
    }

    #[test]
    fn iso_zulu() {
        assert!(parse("2026-09-21T15:00:00Z").is_ok());
    }

    #[test]
    fn day_only() {
        let d = parse("2026-09-21").unwrap();
        assert_eq!(d.day(), 21);
        assert_eq!(d.hour(), 0);
    }

    #[test]
    fn datetime_space() {
        let d = parse("2026-09-21 15:30").unwrap();
        assert_eq!(d.hour(), 15);
        assert_eq!(d.minute(), 30);
    }

    #[test]
    fn in_hours() {
        let d = parse("in 2h").unwrap();
        let diff = d - Local::now();
        assert!(diff > Duration::minutes(110) && diff < Duration::minutes(130));
    }

    #[test]
    fn tomorrow_afternoon() {
        let d = parse("tomorrow 3pm").unwrap();
        let tomorrow = (Local::now() + Duration::days(1)).date_naive();
        assert_eq!(d.date_naive(), tomorrow);
        assert_eq!(d.hour(), 15);
    }

    #[test]
    fn clock_variants() {
        assert_eq!(parse_clock("3pm"), Some((15, 0)));
        assert_eq!(parse_clock("15:00"), Some((15, 0)));
        assert_eq!(parse_clock("9.30am"), Some((9, 30)));
        assert_eq!(parse_clock("0900"), Some((9, 0)));
        assert_eq!(parse_clock("12am"), Some((0, 0)));
    }

    #[test]
    fn garbage_rejected() {
        assert!(parse("sometime next week").is_err());
    }
}
