//! Exact 64-bit keys for ID strings.
//!
//! A string of 1 to 10 bytes from `[0-9A-Z_a-z]` packs into 60 bits, 6 bits
//! per byte, big-endian, padded with code 0. Codes keep ASCII order and the pad
//! sorts below every character, so packed keys compare exactly like the
//! strings do, byte by byte. Every other non-empty string is interned and gets
//! `FALLBACK | index`; comparisons involving one go through the bytes.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

pub const FALLBACK: u64 = 1 << 63;

const fn code_table() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut c = 0u8;
    let mut i = 0;
    while i < 256 {
        let b = i as u8;
        if b.is_ascii_digit() || b.is_ascii_uppercase() || b == b'_' || b.is_ascii_lowercase() {
            c += 1;
            t[i] = c;
        }
        i += 1;
    }
    t
}
static CODE: [u8; 256] = code_table();

const fn decode_table() -> [u8; 64] {
    let mut t = [0u8; 64];
    let mut i = 0;
    while i < 256 {
        let c = code_table()[i];
        if c != 0 {
            t[c as usize] = i as u8;
        }
        i += 1;
    }
    t
}
static DECODE: [u8; 64] = decode_table();

#[inline(always)]
pub fn pack(s: &[u8]) -> Option<u64> {
    let n = s.len();
    if n == 0 || n > 10 {
        return None;
    }
    let mut k = 0u64;
    let mut ok = 0xffu8;
    for &b in s {
        let c = CODE[b as usize];
        ok &= if c == 0 { 0 } else { 0xff };
        k = (k << 6) | c as u64;
    }
    if ok == 0 {
        return None;
    }
    Some(k << (6 * (10 - n)))
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
    let mut v = k;
    let mut buf = [0u8; 10];
    let mut n = 0;
    for i in 0..10 {
        let c = (v >> 54) as usize & 63;
        v <<= 6;
        if c == 0 {
            break;
        }
        buf[i] = DECODE[c];
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
