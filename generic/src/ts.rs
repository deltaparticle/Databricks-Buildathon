//! Timestamps F1 `YYYY-MM-DDTHH:MM:SSZ`, F2 `YYYY-MM-DDTHH:MM:SS±HH:MM` and F3
//! (ASCII digits, epoch milliseconds) to UTC milliseconds. Anything else is bad.

pub const SECOND: i64 = 1_000;
pub const DAY: i64 = 86_400_000;

#[cfg(test)]
#[inline]
fn two(b: &[u8], i: usize) -> Option<i64> {
    let hi = b[i].wrapping_sub(b'0');
    let lo = b[i + 1].wrapping_sub(b'0');
    if hi < 10 && lo < 10 {
        Some(hi as i64 * 10 + lo as i64)
    } else {
        None
    }
}

#[cfg(test)]
fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

#[cfg(test)]
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

pub const fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    era * 146_097 + year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year - 719_468
}

/// The straightforward parser, kept as the oracle the fast one is tested against.
#[cfg(test)]
pub fn parse_slow(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if n == 0 {
        return None;
    }
    if b.iter().all(u8::is_ascii_digit) {
        return parse_epoch(b);
    }
    let offset = match n {
        20 => {
            if b[19] != b'Z' {
                return None;
            }
            0
        }
        25 => {
            let sign = match b[19] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if b[22] != b':' {
                return None;
            }
            let hours = two(b, 20)?;
            let minutes = two(b, 23)?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            sign * (hours * 60 + minutes) * 60_000
        }
        _ => return None,
    };
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let year = two(b, 0)? * 100 + two(b, 2)?;
    let month = two(b, 5)?;
    let day = two(b, 8)?;
    let hour = two(b, 11)?;
    let minute = two(b, 14)?;
    let second = two(b, 17)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let local = ((days_from_civil(year, month, day) * 24 + hour) * 60 + minute) * 60_000 + second * SECOND;
    Some(local - offset)
}

const TABLE_FIRST_YEAR: i64 = 1900;
const TABLE_YEARS: usize = 300;

/// Day number of the first of each month, 1900-01 to 2199-12.
static MONTH_START: [i32; TABLE_YEARS * 12] = build_month_start();

const fn build_month_start() -> [i32; TABLE_YEARS * 12] {
    let mut table = [0i32; TABLE_YEARS * 12];
    let mut i = 0;
    while i < TABLE_YEARS * 12 {
        table[i] = days_from_civil(TABLE_FIRST_YEAR + (i / 12) as i64, (i % 12) as i64 + 1, 1) as i32;
        i += 1;
    }
    table
}

const DAYS_IN_MONTH: [u8; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// `YYYY-MM-DDTHH:MM:SS` (the first 19 bytes of F1 and F2) as local milliseconds.
#[inline(always)]
fn civil_ms(b: &[u8; 19]) -> Option<i64> {
    let d = |i: usize| b[i].wrapping_sub(b'0') as u32;
    let digits = [d(0), d(1), d(2), d(3), d(5), d(6), d(8), d(9), d(11), d(12), d(14), d(15), d(17), d(18)];
    let mut bad = (b[4] != b'-') | (b[7] != b'-') | (b[10] != b'T') | (b[13] != b':') | (b[16] != b':');
    for x in digits {
        bad |= x > 9;
    }
    let year = digits[0] * 1000 + digits[1] * 100 + digits[2] * 10 + digits[3];
    let month = digits[4] * 10 + digits[5];
    let day = digits[6] * 10 + digits[7];
    let hour = digits[8] * 10 + digits[9];
    let minute = digits[10] * 10 + digits[11];
    let second = digits[12] * 10 + digits[13];
    bad |= month.wrapping_sub(1) > 11 || day == 0 || hour > 23 || minute > 59 || second > 59;
    if bad {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = DAYS_IN_MONTH[(month - 1) as usize] as u32 + (month == 2 && leap) as u32;
    if day > days_in_month {
        return None;
    }
    let (year, month, day) = (year as i64, month as i64, day as i64);
    let index = (year - TABLE_FIRST_YEAR) * 12 + month - 1;
    let days = if (0..(TABLE_YEARS * 12) as i64).contains(&index) {
        MONTH_START[index as usize] as i64 + day - 1
    } else {
        days_from_civil(year, month, day)
    };
    Some(((days * 24 + hour as i64) * 60 + minute as i64) * 60_000 + second as i64 * SECOND)
}

/// UTC milliseconds, or None for a bad timestamp. An F3 value that does not fit in
/// an i64 is returned as None: it lies outside every window anyway.
#[inline]
pub fn parse(b: &[u8]) -> Option<i64> {
    match b.len() {
        0 => None,
        20 if b[4] == b'-' => {
            if b[19] != b'Z' {
                return None;
            }
            civil_ms(b[..19].try_into().unwrap())
        }
        25 if b[4] == b'-' => {
            let sign = match b[19] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (h0, h1, m0, m1) = (
                b[20].wrapping_sub(b'0'),
                b[21].wrapping_sub(b'0'),
                b[23].wrapping_sub(b'0'),
                b[24].wrapping_sub(b'0'),
            );
            if b[22] != b':' || h0 > 9 || h1 > 9 || m0 > 9 || m1 > 9 {
                return None;
            }
            let (hours, minutes) = ((h0 * 10 + h1) as i64, (m0 * 10 + m1) as i64);
            if hours > 23 || minutes > 59 {
                return None;
            }
            civil_ms(b[..19].try_into().unwrap()).map(|local| local - sign * (hours * 60 + minutes) * 60_000)
        }
        1..=18 => {
            // F3 that cannot overflow: one pass checks and converts.
            let mut value: i64 = 0;
            for &c in b {
                let x = c.wrapping_sub(b'0');
                if x > 9 {
                    return None;
                }
                value = value * 10 + x as i64;
            }
            Some(value)
        }
        _ => parse_epoch(b),
    }
}

#[inline]
fn parse_epoch(b: &[u8]) -> Option<i64> {
    if !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let first = b.iter().position(|&c| c != b'0').unwrap_or(b.len());
    let digits = &b[first..];
    if digits.len() > 19 {
        return None;
    }
    let mut value: u64 = 0;
    for &c in digits {
        value = value * 10 + (c - b'0') as u64;
    }
    if value > i64::MAX as u64 {
        None
    } else {
        Some(value as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f1(s: &str) -> i64 {
        parse(s.as_bytes()).unwrap()
    }

    #[test]
    fn valid_forms() {
        assert_eq!(f1("2026-10-03T23:58:10Z"), 1_791_071_890_000);
        assert_eq!(f1("2026-10-03T19:58:10-04:00"), 1_791_071_890_000);
        assert_eq!(f1("1791071890000"), 1_791_071_890_000);
        assert_eq!(f1("0001791071890000"), 1_791_071_890_000);
        assert_eq!(f1("000000000000000000000000001791071890000"), 1_791_071_890_000);
        assert_eq!(f1("2026-10-04T01:30:00+02:00"), f1("2026-10-03T23:30:00Z"));
        assert_eq!(f1("2026-10-03T10:00:00-00:00"), f1("2026-10-03T10:00:00Z"));
        assert_eq!(f1("2026-10-03T10:00:00+23:59"), f1("2026-10-02T10:01:00Z"));
        assert_eq!(f1("2028-02-29T00:00:00Z"), f1("2028-02-28T00:00:00Z") + DAY);
        assert_eq!(f1("2000-02-29T00:00:00Z"), f1("2000-02-28T00:00:00Z") + DAY);
        assert_eq!(f1("86400000"), DAY);
        assert_eq!(f1("0"), 0);
        assert_eq!(f1("1970-01-01T00:00:00Z"), 0);
        assert_eq!(f1("9223372036854775807"), i64::MAX);
        assert_eq!(f1("2027-01-01T05:00:00+12:00"), f1("2026-12-31T17:00:00Z"));
    }

    #[test]
    fn fast_matches_slow() {
        let mut s = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let seeds = [
            "2026-10-03T23:58:10Z", "2028-02-29T00:00:00Z", "2027-02-28T23:59:59Z", "1999-12-31T23:59:59-10:00",
            "2025-01-01T00:00:00+12:00", "0000-01-01T00:00:00Z", "9999-12-31T23:59:59Z", "1791071890000",
            "00000000000001791071890000", "2100-02-28T12:00:00+05:45", "1900-03-01T00:00:00-00:00",
        ];
        let pool = b"0123456789-+:TZtz .\r";
        for round in 0..400_000u64 {
            let mut v: Vec<u8> = seeds[(round % seeds.len() as u64) as usize].as_bytes().to_vec();
            let edits = next() % 4;
            for _ in 0..edits {
                let i = (next() % v.len() as u64) as usize;
                match next() % 5 {
                    0 => v[i] = pool[(next() % pool.len() as u64) as usize],
                    1 => v[i] = b'0' + (next() % 10) as u8,
                    2 => {
                        v.remove(i);
                    }
                    3 => v.insert(i, b'0' + (next() % 10) as u8),
                    _ => v[i] = b'9',
                }
                if v.is_empty() {
                    break;
                }
            }
            assert_eq!(parse(&v), parse_slow(&v), "{:?}", String::from_utf8_lossy(&v));
        }
        // every calendar day (valid or not) of 1899..=2200, around both table edges
        for y in 1899..=2200 {
            for m in 0..=13 {
                for d in 0..=32 {
                    for text in [format!("{y:04}-{m:02}-{d:02}T01:02:03Z"), format!("{y:04}-{m:02}-{d:02}T23:59:59-07:30")] {
                        assert_eq!(parse(text.as_bytes()), parse_slow(text.as_bytes()), "{text}");
                    }
                }
            }
        }
    }

    #[test]
    fn bad_forms() {
        for s in [
            "",
            "2026-10-03T23:58:10.000Z",
            "2026-10-03T23:58:10z",
            "2026-10-03t23:58:10Z",
            "2026-10-03 23:58:10Z",
            "2026-10-03T23:58:10",
            "1791071890000.0",
            "1.79e12",
            "-5",
            "+5",
            "2026-13-03T23:58:10Z",
            "2026-00-03T23:58:10Z",
            "2026-02-30T00:00:00Z",
            "2027-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2026-10-00T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-10-03T24:00:00Z",
            "2026-10-03T23:60:00Z",
            "2026-10-03T23:58:60Z",
            "2026-10-03T23:58:10+24:00",
            "2026-10-03T23:58:10+05:60",
            "2026-10-03T23:58:10+0200",
            "2026-10-03T23:58:10 02:00",
            "2026-10-03T23:58:10Z\r",
            "1791071890000\r",
            " 1791071890000",
            "\u{0662}\u{0665}\u{0660}",
            "\u{ff12}\u{ff15}\u{ff10}",
            "２026-10-03T23:58:10Z",
            "9223372036854775808",
            "123456789012345678901",
            "2026-1a-03T23:58:10Z",
            "2026/10/03T23:58:10Z",
        ] {
            assert_eq!(parse(s.as_bytes()), None, "{s:?} should be bad");
        }
    }
}
