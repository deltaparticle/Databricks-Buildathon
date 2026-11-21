//! Timestamps (rules.md "Timestamps"): F1 `YYYY-MM-DDTHH:MM:SSZ`,
//! F2 `YYYY-MM-DDTHH:MM:SS+HH:MM`, F3 ASCII digits (epoch ms). Anything else is bad.

pub const SECOND: i64 = 1_000;
pub const MINUTE: i64 = 60 * SECOND;
pub const DAY: i64 = 86_400_000;

/// An F3 value too long for i64: valid syntax, outside every window.
pub const HUGE: i64 = i64::MAX;

#[inline(always)]
fn d2(s: &[u8], i: usize) -> Option<i64> {
    let a = s[i].wrapping_sub(b'0');
    let b = s[i + 1].wrapping_sub(b'0');
    if a < 10 && b < 10 {
        Some(a as i64 * 10 + b as i64)
    } else {
        None
    }
}

pub const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since the epoch of the first of each month, 1900-01 to 2199-12.
const TABLE_FIRST_YEAR: i64 = 1900;
const TABLE_YEARS: usize = 300;
static MONTH_START: [i32; TABLE_YEARS * 12] = {
    let mut t = [0i32; TABLE_YEARS * 12];
    let mut i = 0;
    while i < TABLE_YEARS * 12 {
        t[i] = days_from_civil(TABLE_FIRST_YEAR + (i / 12) as i64, (i % 12) as i64 + 1, 1) as i32;
        i += 1;
    }
    t
};

#[inline(always)]
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `YYYY-MM-DDTHH:MM:SS` in the first 19 bytes, as local epoch ms.
#[inline(always)]
fn civil19(s: &[u8]) -> Option<i64> {
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let year = d2(s, 0)? * 100 + d2(s, 2)?;
    let month = d2(s, 5)?;
    let day = d2(s, 8)?;
    let hour = d2(s, 11)?;
    let minute = d2(s, 14)?;
    let second = d2(s, 17)?;
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let y = year - TABLE_FIRST_YEAR;
    let days = if (0..TABLE_YEARS as i64).contains(&y) {
        MONTH_START[(y * 12 + month - 1) as usize] as i64 + day - 1
    } else {
        days_from_civil(year, month, day)
    };
    Some(days * DAY + ((hour * 60 + minute) * 60 + second) * SECOND)
}

/// Epoch milliseconds, `HUGE` for an F3 value beyond i64, or None if bad.
#[inline]
pub fn parse(s: &[u8]) -> Option<i64> {
    let n = s.len();
    if n == 20 && s[19] == b'Z' {
        return civil19(s);
    }
    if n == 25 && (s[19] == b'+' || s[19] == b'-') {
        if s[22] != b':' {
            return None;
        }
        let oh = d2(s, 20)?;
        let om = d2(s, 23)?;
        if oh > 23 || om > 59 {
            return None;
        }
        let offset = (oh * 60 + om) * MINUTE;
        let local = civil19(s)?;
        return Some(if s[19] == b'+' { local - offset } else { local + offset });
    }
    parse_f3(s)
}

#[inline]
fn parse_f3(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    let mut significant = 0usize;
    for &b in s {
        let d = b.wrapping_sub(b'0');
        if d >= 10 {
            return None;
        }
        if significant > 0 || d != 0 {
            significant += 1;
            if significant <= 18 {
                value = value * 10 + d as u64;
            }
        }
    }
    if significant > 18 {
        // Could still fit in i64 at 19 digits, but any such value is far
        // outside every window (year > 2286), so it is simply out of window.
        return Some(HUGE);
    }
    Some(value as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forms() {
        let f1 = parse(b"2026-10-03T23:58:10Z").unwrap();
        assert_eq!(parse(b"2026-10-03T19:58:10-04:00"), Some(f1));
        assert_eq!(parse(b"1791071890000"), Some(1791071890000));
        assert_eq!(parse(b"0001791071890000"), Some(1791071890000));
        assert_eq!(parse(b"2026-10-04T01:30:00+02:00"), parse(b"2026-10-03T23:30:00Z"));
        for bad in [
            &b""[..], b"2026-10-03 10:00:00Z", b"2026-10-03T10:00:00z", b"2026-10-03t10:00:00Z",
            b"2026-13-03T10:00:00Z", b"2026-02-30T10:00:00Z", b"2027-02-29T10:00:00Z",
            b"2026-10-03T24:00:00Z", b"2026-10-03T10:00:60Z", b"2026-10-03T10:00:00+24:00",
            b"2026-10-03T10:00:00+05:60", b"2026-10-03T10:00:00+0200", b"1791071890000.0",
            b"1.79e12", b"-5", b"2026-10-03T10:00:00", b"2026-10-03T10:00:00.123Z",
        ] {
            assert_eq!(parse(bad), None, "{:?}", String::from_utf8_lossy(bad));
        }
        assert!(parse(b"2028-02-29T10:00:00Z").is_some());
        for y in [1899i64, 1900, 1970, 2000, 2025, 2028, 2100, 2199, 2200] {
            for m in 1..=12 {
                let txt = format!("{y:04}-{m:02}-15T12:34:56Z");
                assert_eq!(parse(txt.as_bytes()), Some(days_from_civil(y, m, 15) * DAY + 45_296_000), "{txt}");
            }
        }
        assert_eq!(parse(b"123456789012345678901"), Some(HUGE));
        assert_eq!(parse(b"2026-10-03T10:00:00-00:00"), parse(b"2026-10-03T10:00:00Z"));
    }
}
