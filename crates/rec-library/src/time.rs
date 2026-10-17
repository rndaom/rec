//! Wall-clock and clip-time text.

/// Seconds since the Unix epoch of an RFC 3339 time such as
/// `2026-10-17T21:04:03.112Z` or `2026-10-17T17:04:03-04:00`. A bare date
/// (`2026-10-17`) is midnight UTC, and a time without a zone is UTC.
pub fn parse_utc(text: &str) -> Option<f64> {
    let text = text.trim();
    let (date, rest) = text.split_at(text.len().min(10));
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = days_from_civil(y, m, d) as f64 * 86_400.0;
    let rest = rest.trim_start_matches(['T', 't', ' ']);
    if rest.is_empty() {
        return Some(secs);
    }
    // Split off the zone: `Z`, or a sign after the clock.
    let (clock, zone) = match rest.find(['Z', 'z', '+', '-']) {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    let mut fields = clock.split(':');
    let h: f64 = fields.next()?.parse().ok()?;
    let min: f64 = fields.next()?.parse().ok()?;
    let s: f64 = fields.next().map_or(Some(0.0), |s| s.parse().ok())?;
    if fields.next().is_some() || h >= 24.0 || min >= 60.0 || !(0.0..61.0).contains(&s) {
        return None;
    }
    secs += h * 3600.0 + min * 60.0 + s;
    match zone {
        "" | "Z" | "z" => {}
        _ => {
            let sign = if zone.starts_with('-') { 1.0 } else { -1.0 };
            let (zh, zm) = zone[1..].split_once(':').unwrap_or((&zone[1..], "0"));
            let offset = zh.parse::<f64>().ok()? * 3600.0 + zm.parse::<f64>().ok()? * 60.0;
            secs += sign * offset;
        }
    }
    Some(secs)
}

/// `2026-10-17T21:04:03.112Z`.
pub fn format_utc(unix: f64) -> String {
    let ms = (unix * 1000.0).round() as i64;
    let (days, ms_of_day) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, m, d) = civil_from_days(days);
    let s = ms_of_day / 1000;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        s / 3600,
        s / 60 % 60,
        s % 60,
        ms_of_day % 1000
    )
}

/// Local wall-clock time for people, e.g. `2026-10-17 17:04:03 (UTC-04:00)`.
pub fn format_local(unix: f64, offset_min: i32) -> String {
    let local = unix.floor() as i64 + i64::from(offset_min) * 60;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    let s = local.rem_euclid(86_400);
    let sign = if offset_min < 0 { '-' } else { '+' };
    let off = offset_min.unsigned_abs();
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} (UTC{sign}{:02}:{:02})",
        s / 3600,
        s / 60 % 60,
        s % 60,
        off / 60,
        off % 60
    )
}

/// Clip or session time for people: `0:21.2`, `12:03.0`, `1:02:03.4`.
pub fn clock(t: f64) -> String {
    let tenths = (t.max(0.0) * 10.0).round() as u64;
    let (s, tenth) = (tenths / 10, tenths % 10);
    if s >= 3600 {
        format!("{}:{:02}:{:02}.{tenth}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}.{tenth}", s / 60, s % 60)
    }
}

/// File stem of a still at clip time `t`: `t021.18`. Names sort by time up
/// to 999 s.
pub fn still_name(t: f64) -> String {
    format!("t{:06.2}", t.max(0.0))
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_utc() {
        let t = parse_utc("2026-10-17T21:04:03.112Z").unwrap();
        assert_eq!(format_utc(t), "2026-10-17T21:04:03.112Z");
        assert_eq!(parse_utc("2026-10-17T17:04:03.112-04:00"), Some(t));
        assert_eq!(parse_utc("2026-10-18T02:34:03.112+05:30"), Some(t));
        assert_eq!(parse_utc("1970-01-01"), Some(0.0));
        assert_eq!(parse_utc("2000-03-01"), Some(951_868_800.0));
        assert_eq!(
            parse_utc("2026-10-17 21:04"),
            parse_utc("2026-10-17T21:04:00Z")
        );
        for bad in [
            "",
            "2026-13-01",
            "2026-10",
            "yesterday",
            "2026-10-17T25:00Z",
        ] {
            assert_eq!(parse_utc(bad), None, "{bad}");
        }
    }

    #[test]
    fn formats_local_time() {
        let t = parse_utc("2026-10-17T21:04:03.9Z").unwrap();
        assert_eq!(format_local(t, -240), "2026-10-17 17:04:03 (UTC-04:00)");
        assert_eq!(format_local(t, 330), "2026-10-18 02:34:03 (UTC+05:30)");
    }

    #[test]
    fn clocks_and_still_names() {
        assert_eq!(clock(21.18), "0:21.2");
        assert_eq!(clock(723.0), "12:03.0");
        assert_eq!(clock(3723.44), "1:02:03.4");
        assert_eq!(clock(59.96), "1:00.0");
        assert_eq!(still_name(21.18), "t021.18");
        assert_eq!(still_name(0.0), "t000.00");
        assert_eq!(still_name(125.004), "t125.00");
    }
}
