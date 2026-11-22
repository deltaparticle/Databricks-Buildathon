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

/// Year, month, hour, minute and second of the words "YYYY-MM-" and "HH:MM:SS"
/// (digits not checked here): each pair of digits is combined in one multiply.
#[inline(always)]
fn fields(date: u64, time: u64) -> (u32, u32, u32, u32, u32) {
    const LOW: u64 = 0x0f0f_0f0f_0f0f_0f0f;
    // byte i of w * 10 + (w >> 8) is digit i * 10 + digit i + 1 (below 100: no carry)
    let pairs = |w: u64| (w & LOW).wrapping_mul(10).wrapping_add((w & LOW) >> 8);
    let (d, t) = (pairs(date), pairs(time));
    let byte = |w: u64, i: u32| ((w >> (8 * i)) & 0xff) as u32;
    (byte(d, 0) * 100 + byte(d, 2), byte(d, 5), byte(t, 0), byte(t, 3), byte(t, 6))
}

/// `YYYY-MM-DDTHH:MM:SS` (the first 19 bytes of F1 and F2) as local milliseconds.
#[inline(always)]
fn civil_ms(b: &[u8; 19]) -> Option<i64> {
    // Two little-endian words: "YYYY-MM-" (bytes 0-7) and "HH:MM:SS" (bytes 11-18).
    // Digits are checked nibble-wise (high nibble 3, low nibble + 6 < 16), which
    // never carries between bytes; separators by masked comparison.
    const HIGH: u64 = 0xf0f0_f0f0_f0f0_f0f0;
    const LOW: u64 = 0x0f0f_0f0f_0f0f_0f0f;
    let digits_ok = |w: u64, m: u64| {
        ((w & HIGH ^ 0x3030_3030_3030_3030) & m) | (((w & LOW) + 0x0606_0606_0606_0606) & 0x1010_1010_1010_1010 & m) == 0
    };
    let date = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let time = u64::from_le_bytes(b[11..19].try_into().unwrap());
    const DATE_DIGITS: u64 = 0x00ff_ff00_ffff_ffff; // bytes 0,1,2,3,5,6
    const DATE_SEPS: u64 = 0xff00_00ff_0000_0000; // bytes 4,7: '-'
    const TIME_DIGITS: u64 = 0xffff_00ff_ff00_ffff; // bytes 0,1,3,4,6,7
    const TIME_SEPS: u64 = 0x0000_ff00_00ff_0000; // bytes 2,5: ':'
    let (d0, d1) = (b[8].wrapping_sub(b'0') as u32, b[9].wrapping_sub(b'0') as u32);
    let mut bad = !digits_ok(date, DATE_DIGITS)
        | !digits_ok(time, TIME_DIGITS)
        | (date & DATE_SEPS != 0x2d00_002d_0000_0000)
        | (time & TIME_SEPS != 0x0000_3a00_003a_0000)
        | (b[10] != b'T')
        | (d0 > 9)
        | (d1 > 9);
    let (year, month, hour, minute, second) = fields(date, time);
    let day = d0 * 10 + d1;
    bad |= month.wrapping_sub(1) > 11 || day == 0 || hour > 23 || minute > 59 || second > 59;
    if bad {
        return None;
    }
    let index = (year as i64 - TABLE_FIRST_YEAR) * 12 + month as i64 - 1;
    let days = if (0..(TABLE_YEARS * 12 - 1) as i64).contains(&index) {
        // the month's length is the gap to the next month's start: no leap-year arithmetic
        let (first, next) = (MONTH_START[index as usize], MONTH_START[index as usize + 1]);
        if day as i32 > next - first {
            return None;
        }
        first as i64 + day as i64 - 1
    } else {
        let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
        let days_in_month = DAYS_IN_MONTH[(month - 1) as usize] as u32 + (month == 2 && leap) as u32;
        if day > days_in_month {
            return None;
        }
        days_from_civil(year as i64, month as i64, day as i64)
    };
    Some(days * 86_400_000 + hour as i64 * 3_600_000 + minute as i64 * 60_000 + second as i64 * SECOND)
}

/// `ingest_ts`, which rules.md guarantees is a valid F1: checked only for its
/// shape (digits and separators), not for calendar ranges, then converted with
/// independent multiplies. Anything else goes through `parse` (None = i64::MAX).
#[inline(always)]
pub fn ingest(b: &[u8]) -> i64 {
    const HIGH: u64 = 0xf0f0_f0f0_f0f0_f0f0;
    const LOW: u64 = 0x0f0f_0f0f_0f0f_0f0f;
    if b.len() == 20 {
        let date = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let time = u64::from_le_bytes(b[11..19].try_into().unwrap());
        let mid = u32::from_le_bytes(b[8..12].try_into().unwrap()); // "DDTH"
        let digits_ok = |w: u64, m: u64| {
            ((w & HIGH ^ 0x3030_3030_3030_3030) & m) | (((w & LOW) + 0x0606_0606_0606_0606) & 0x1010_1010_1010_1010 & m) == 0
        };
        let (d0, d1) = ((mid & 0xff).wrapping_sub(0x30), (mid >> 8 & 0xff).wrapping_sub(0x30));
        let shape = digits_ok(date, 0x00ff_ff00_ffff_ffff)
            & digits_ok(time, 0xffff_00ff_ff00_ffff)
            & (date & 0xff00_00ff_0000_0000 == 0x2d00_002d_0000_0000)
            & (time & 0x0000_ff00_00ff_0000 == 0x0000_3a00_003a_0000)
            & (mid >> 16 & 0xff == b'T' as u32)
            & (b[19] == b'Z')
            & (d0 < 10)
            & (d1 < 10);
        if shape {
            let (year, month, hour, minute, second) = fields(date, time);
            let index = (year as i64 - TABLE_FIRST_YEAR) * 12 + month as i64 - 1;
            if (0..(TABLE_YEARS * 12) as i64).contains(&index) {
                let days = MONTH_START[index as usize] as i64 + (d0 * 10 + d1) as i64 - 1;
                return days * 86_400_000 + hour as i64 * 3_600_000 + minute as i64 * 60_000 + second as i64 * SECOND;
            }
        }
    }
    parse(b).unwrap_or(i64::MAX)
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
        13 => {
            // The usual F3 (13 digits): two overlapping 8-byte words.
            let hi = u64::from_le_bytes(b[0..8].try_into().unwrap());
            let lo = u64::from_le_bytes(b[5..13].try_into().unwrap());
            let (hi, lo) = (digits8(hi)?, digits8(lo)?);
            Some((hi * 100_000 + lo % 100_000) as i64)
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

/// The 8 ASCII digits of a little-endian word (first digit in the low byte) as a
/// number, or None if any byte is not 0-9.
#[inline(always)]
fn digits8(word: u64) -> Option<u64> {
    let v = word.wrapping_sub(0x3030_3030_3030_3030);
    // a byte is a digit iff v <= 9 there: no borrow into the high bit, and adding
    // 0x76 does not carry into it either
    if (v | v.wrapping_add(0x7676_7676_7676_7676)) & 0x8080_8080_8080_8080 != 0 {
        return None;
    }
    let v = (v.wrapping_mul(10) + (v >> 8)) & 0x00ff_00ff_00ff_00ff;
    let v = (v.wrapping_mul(100) + (v >> 16)) & 0x0000_ffff_0000_ffff;
    Some((v.wrapping_mul(10_000) + (v >> 32)) & 0xffff_ffff)
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
    #[test]
    fn ingest_matches_parse_on_valid_f1() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..300_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let year = 1900 + (x % 300) as i64;
            let month = 1 + (x >> 9) % 12;
            let dim = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][(month - 1) as usize]
                + (month == 2 && year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)) as u64;
            let day = 1 + (x >> 17) % dim;
            let text = format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", (x >> 23) % 24, (x >> 29) % 60, (x >> 35) % 60);
            assert_eq!(super::ingest(text.as_bytes()), super::parse(text.as_bytes()).unwrap(), "{text}");
        }
        for odd in ["", "2028-01-01T00:00:00", "2028-01-01 00:00:00Z", "1832574986000", "2028-01-01T00:00:00+01:00", "2399-01-01T00:00:00Z"] {
            assert_eq!(super::ingest(odd.as_bytes()), super::parse(odd.as_bytes()).unwrap_or(i64::MAX), "{odd}");
        }
    }

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
    fn digits8_words() {
        assert_eq!(digits8(u64::from_le_bytes(*b"12345678")), Some(12_345_678));
        assert_eq!(digits8(u64::from_le_bytes(*b"00000000")), Some(0));
        assert_eq!(digits8(u64::from_le_bytes(*b"99999999")), Some(99_999_999));
        assert_eq!(digits8(u64::from_le_bytes([b'1', b'2', b'3', b'4', b'5', b'6', 0xff, b'8'])), None);
        for bad in [*b"1234567:", *b"/2345678", *b"1234 678"] {
            assert_eq!(digits8(u64::from_le_bytes(bad)), None, "{:?}", bad);
        }
        // every single-byte corruption of a valid word
        for pos in 0..8 {
            for byte in 0..=255u8 {
                let mut w = *b"31415926";
                w[pos] = byte;
                let expect = w.iter().all(u8::is_ascii_digit).then(|| std::str::from_utf8(&w).unwrap().parse::<u64>().unwrap());
                assert_eq!(digits8(u64::from_le_bytes(w)), expect, "{:?}", w);
            }
        }
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
