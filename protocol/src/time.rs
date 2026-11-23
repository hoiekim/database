//! Dates and times without a calendar library: timestamps are microseconds
//! since the Unix epoch in UTC, dates are days since it.

pub const SECOND_MICROS: i64 = 1_000_000;
pub const DAY_MICROS: i64 = 86_400 * SECOND_MICROS;

/// The first microsecond of a day, counted in days since the epoch.
/// `None` for a day so far off that its microseconds do not fit: a date
/// reaches five million years either way, a timestamp three hundred
/// thousand.
pub fn day_start(days: i32) -> Option<i64> {
    i64::from(days).checked_mul(DAY_MICROS)
}

pub fn now_micros() -> i64 {
    system_micros(std::time::SystemTime::now())
}

pub fn system_micros(t: std::time::SystemTime) -> i64 {
    match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_micros() as i64,
        Err(e) => -(e.duration().as_micros() as i64),
    }
}

fn is_leap(year: i32) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

/// Days since the epoch of a calendar day, or `None` if there is no such
/// day (30 February).
pub fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i32> {
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) || !(-9999..=9999).contains(&year) {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = i64::from(if month <= 2 { year - 1 } else { year });
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) as i32)
}

pub fn civil_from_days(days: i32) -> (i32, u32, u32) {
    let z = i64::from(days) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year as i32, month, day)
}

pub fn format_date(days: i32) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// RFC 3339 in UTC, always with six fraction digits.
pub fn format_timestamp(micros: i64) -> String {
    let days = micros.div_euclid(DAY_MICROS);
    let rest = micros.rem_euclid(DAY_MICROS);
    let (y, mo, d) = civil_from_days(days as i32);
    let secs = rest / SECOND_MICROS;
    format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:06}Z", secs / 3600, secs / 60 % 60, secs % 60, rest % SECOND_MICROS)
}

fn digits(s: &[u8], n: usize) -> Option<(u32, &[u8])> {
    if s.len() < n || !s[..n].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let v = s[..n].iter().fold(0u32, |acc, c| acc * 10 + u32::from(c - b'0'));
    Some((v, &s[n..]))
}

fn eat(s: &[u8], c: u8) -> Option<&[u8]> {
    (s.first() == Some(&c)).then(|| &s[1..])
}

fn ymd(s: &[u8]) -> Option<(i32, &[u8])> {
    let (y, s) = digits(s, 4)?;
    let s = eat(s, b'-')?;
    let (m, s) = digits(s, 2)?;
    let s = eat(s, b'-')?;
    let (d, s) = digits(s, 2)?;
    Some((days_from_civil(y as i32, m, d)?, s))
}

/// `YYYY-MM-DD`, and only a day that exists.
pub fn parse_date(text: &str) -> Option<i32> {
    match ymd(text.trim().as_bytes())? {
        (days, []) => Some(days),
        _ => None,
    }
}

const MONTHS: [&str; 12] =
    ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];

fn month_named(word: &str) -> Option<u32> {
    let word = word.to_ascii_lowercase();
    let word = word.trim_end_matches('.');
    if word.len() < 3 {
        return None;
    }
    MONTHS.iter().position(|m| *m == word || (word.len() == 3 && m.starts_with(word))).map(|i| i as u32 + 1)
}

/// `March 5, 2026`, `Mar 5 2026`, `5 March 2026`.
fn parse_written_date(text: &str) -> Option<i32> {
    let words: Vec<&str> = text.split([' ', ',']).filter(|w| !w.is_empty()).collect();
    let [a, b, year] = words[..] else { return None };
    let year: i32 = year.parse().ok()?;
    let (month, day) = match month_named(a) {
        Some(month) => (month, b.parse().ok()?),
        None => (month_named(b)?, a.parse().ok()?),
    };
    days_from_civil(year, month, day)
}

/// Reads a point in time. Takes RFC 3339 and what Postgres prints:
/// `YYYY-MM-DD`, then optionally `T` or a space and `HH:MM[:SS[.fraction]]`,
/// then optionally a zone (`Z`, `+HH`, `+HH:MM`, `+HHMM`). Also a written
/// date such as `March 5, 2026`. Without a zone the time is taken to be
/// `default_offset_secs` east of UTC. `None` for anything else, including
/// a day that does not exist.
pub fn parse_timestamp(text: &str, default_offset_secs: i32) -> Option<i64> {
    let text = text.trim();
    let midnight = |days: i32| day_start(days)?.checked_sub(i64::from(default_offset_secs) * SECOND_MICROS);
    let Some((days, rest)) = ymd(text.as_bytes()) else {
        return parse_written_date(text).and_then(midnight);
    };
    if rest.is_empty() {
        return midnight(days);
    }
    let rest = eat(rest, b'T').or_else(|| eat(rest, b't')).or_else(|| eat(rest, b' '))?;
    let (hour, rest) = digits(rest, 2)?;
    let rest = eat(rest, b':')?;
    let (minute, mut rest) = digits(rest, 2)?;
    let mut second = 0;
    let mut micros = 0i64;
    if let Some(after) = eat(rest, b':') {
        (second, rest) = digits(after, 2)?;
        if let Some(after) = eat(rest, b'.') {
            let n = after.iter().take_while(|c| c.is_ascii_digit()).count();
            if n == 0 {
                return None;
            }
            for (i, c) in after[..n].iter().enumerate().take(6) {
                micros += i64::from(c - b'0') * 10i64.pow(5 - i as u32);
            }
            rest = &after[n..];
        }
    }
    // 24:00 is the end of the day, as ISO 8601 allows.
    let end_of_day = hour == 24 && minute == 0 && second == 0 && micros == 0;
    if (hour > 23 && !end_of_day) || minute > 59 || second > 60 {
        return None;
    }
    while let Some(after) = eat(rest, b' ') {
        rest = after;
    }
    let offset_secs = match rest {
        [] => i64::from(default_offset_secs),
        [b'Z'] | [b'z'] => 0,
        [sign @ (b'+' | b'-'), zone @ ..] => {
            let (h, zone) = digits(zone, 2)?;
            let zone = eat(zone, b':').unwrap_or(zone);
            let (m, zone) = if zone.is_empty() { (0, zone) } else { digits(zone, 2)? };
            if !zone.is_empty() || h > 15 || m > 59 {
                return None;
            }
            let secs = i64::from(h * 3600 + m * 60);
            if *sign == b'-' {
                -secs
            } else {
                secs
            }
        }
        _ => return None,
    };
    let of_day = i64::from(hour * 3600 + minute * 60 + second) * SECOND_MICROS + micros;
    day_start(days)?.checked_add(of_day)?.checked_sub(offset_secs * SECOND_MICROS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_and_dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(2000, 3, 1), Some(11_017));
        assert_eq!(days_from_civil(1969, 12, 31), Some(-1));
        for days in (-800_000..800_000).step_by(997) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), Some(days));
        }
        assert_eq!(format_date(20_517), "2026-03-05");
        assert_eq!(parse_date("2024-02-29"), Some(19_782));
        assert_eq!(parse_date("2026-02-29"), None);
        assert_eq!(parse_date("2026-02-30"), None);
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_date("2026-3-5"), None);
    }

    #[test]
    fn timestamps_print_as_rfc_3339() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00.000000Z");
        assert_eq!(format_timestamp(1_772_706_030_123_456), "2026-03-05T10:20:30.123456Z");
        assert_eq!(format_timestamp(-1), "1969-12-31T23:59:59.999999Z");
    }

    #[test]
    fn timestamps_parse_in_the_forms_the_apps_send() {
        let t = 1_772_706_030_123_456;
        assert_eq!(parse_timestamp("2026-03-05T10:20:30.123456Z", 0), Some(t));
        assert_eq!(parse_timestamp("2026-03-05 10:20:30.123456+00", 0), Some(t));
        assert_eq!(parse_timestamp("2026-03-05T12:20:30.123456+02:00", 0), Some(t));
        assert_eq!(parse_timestamp("2026-03-05T05:50:30.123456-0430", 0), Some(t));
        assert_eq!(parse_timestamp("2026-03-05T10:20:30.1234567Z", 0), Some(t));
        assert_eq!(parse_timestamp("2026-03-05T10:20Z", 0), Some(t - 30_123_456));
        assert_eq!(parse_timestamp("2026-03-05", 0), Some(20_517 * DAY_MICROS));
        assert_eq!(parse_timestamp("2026-03-05", 3600), Some(20_517 * DAY_MICROS - 3600 * SECOND_MICROS));
        assert_eq!(parse_timestamp("2026-03-05T24:00:00Z", 0), Some(20_518 * DAY_MICROS));
        assert_eq!(parse_timestamp("March 5, 2026", 0), Some(20_517 * DAY_MICROS));
        assert_eq!(parse_timestamp("5 Mar 2026", 0), Some(20_517 * DAY_MICROS));
        for bad in ["", "2026-02-30", "2026-02-30T00:00:00Z", "2026-03-05T25:00:00Z", "2026-03-05T10:61:00Z", "yesterday", "2026-03-05T10:20:30+99", "Marchember 5, 2026"] {
            assert_eq!(parse_timestamp(bad, 0), None, "{bad}");
        }
        assert_eq!(parse_timestamp(&format_timestamp(t), 0), Some(t));
    }
}
