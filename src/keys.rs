//! Exact 64-bit keys for ID strings.
//!
//! A string of 1 to 9 ASCII bytes with no NUL packs into 63 bits: 7 bits per
//! byte, big-endian, padded with 0. ASCII order is kept and the pad sorts
//! below every byte, so packed keys compare exactly like the strings do, byte
//! by byte. Every other non-empty string is interned and gets
//! `FALLBACK | index`; comparisons involving one go through the bytes.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

pub const FALLBACK: u64 = 1 << 63;

/// Pack up to 9 ASCII bytes (no NUL), or None. Branch-light SWAR.
#[inline(always)]
pub fn pack(s: &[u8]) -> Option<u64> {
    let n = s.len();
    if n == 0 || n > 9 {
        return None;
    }
    let mut b = [0u8; 16];
    b[..n].copy_from_slice(s);
    let hi = u64::from_be_bytes(b[..8].try_into().unwrap());
    let lo = b[8] as u64;
    // ASCII only.
    if (hi | lo) & 0x8080_8080_8080_8080 != 0 {
        return None;
    }
    // No NUL inside the string: make the padding bytes non-zero, then test
    // for any zero byte (exact for presence).
    let pad = if n >= 8 { 0 } else { u64::MAX >> (8 * n) };
    let v = hi | pad;
    if (v.wrapping_sub(0x0101_0101_0101_0101) & !v & 0x8080_8080_8080_8080) != 0 || (n == 9 && lo == 0) {
        return None;
    }
    // Squeeze eight 7-bit bytes into 56 bits, keeping their order.
    let mut x = hi;
    x = (x & 0x007F_007F_007F_007F) | ((x & 0x7F00_7F00_7F00_7F00) >> 1);
    x = (x & 0x0000_3FFF_0000_3FFF) | ((x & 0x3FFF_0000_3FFF_0000) >> 2);
    x = (x & 0x0000_0000_0FFF_FFFF) | ((x & 0x0FFF_FFFF_0000_0000) >> 4);
    Some(x << 7 | lo)
}

struct Table {
    map: HashMap<Vec<u8>, u64>,
}

static TABLE: Mutex<Option<Table>> = Mutex::new(None);
static FROZEN: RwLock<Vec<Vec<u8>>> = RwLock::new(Vec::new());

/// The key of a non-empty string, interning it if it does not pack.
#[inline(always)]
pub fn key(s: &[u8]) -> u64 {
    match pack(s) {
        Some(k) => k,
        None => intern(s),
    }
}

#[cold]
fn intern(s: &[u8]) -> u64 {
    let mut guard = TABLE.lock().unwrap();
    let table = guard.get_or_insert_with(|| Table { map: HashMap::new() });
    if let Some(&k) = table.map.get(s) {
        return k;
    }
    let mut frozen = FROZEN.write().unwrap();
    let k = FALLBACK | frozen.len() as u64;
    frozen.push(s.to_vec());
    table.map.insert(s.to_vec(), k);
    k
}

/// Append the string behind `k` to `out`.
#[inline]
pub fn bytes(k: u64, out: &mut Vec<u8>) {
    if k & FALLBACK != 0 {
        out.extend_from_slice(&FROZEN.read().unwrap()[(k & !FALLBACK) as usize]);
        return;
    }
    let mut buf = [0u8; 9];
    let mut n = 0;
    for i in 0..9 {
        let c = ((k >> (7 * (8 - i))) & 0x7F) as u8;
        if c == 0 {
            break;
        }
        buf[i] = c;
        n += 1;
    }
    out.extend_from_slice(&buf[..n]);
}

/// Byte-wise comparison of the strings behind two keys.
#[inline]
pub fn cmp(a: u64, b: u64) -> Ordering {
    if (a | b) & FALLBACK == 0 {
        return a.cmp(&b);
    }
    cmp_slow(a, b)
}

#[cold]
fn cmp_slow(a: u64, b: u64) -> Ordering {
    let (mut x, mut y) = (Vec::new(), Vec::new());
    bytes(a, &mut x);
    bytes(b, &mut y);
    x.cmp(&y)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn order_and_roundtrip() {
        let words: Vec<&[u8]> = vec![
            b"u1", b"U1", b"u", b"uz", b"u10", b"_", b"a", b"0", b"zzzzzzzzzz", b" u1", b"u1 ",
            b"u1\r", b"abcdefghijk", b"user_id_a", b"user_id_b\t", b"p\xc3\xa9",
            b"uzzzzzzzz", b"u00000000", b"u0000000", b"e~~~~~~~~", b"\x7f", b"a\x00b", b"\x00",
            b"ab\x00", b"123456789", b"1234567890", b"U1ABCDEFG", b" ", b"  ",
        ];
        for a in &words {
            let mut out = Vec::new();
            bytes(key(a), &mut out);
            assert_eq!(&out[..], *a);
            for b in &words {
                assert_eq!(cmp(key(a), key(b)), a.cmp(b), "{:?} {:?}", a, b);
                assert_eq!(key(a) == key(b), a == b);
            }
        }
    }
}
