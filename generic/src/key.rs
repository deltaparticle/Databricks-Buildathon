//! IDs as exact 64-bit keys.
//!
//! An ID of 1 to 10 bytes, all in `[0-9A-Z_a-z]`, is packed 6 bits per byte, first
//! byte highest, zero-padded: the codes rise with the byte values and padding (0)
//! sorts before every code, so comparing two packed keys as integers is exactly
//! comparing the IDs byte by byte. Any other ID (empty is never encoded) goes to a
//! global interner and gets bit 63 plus its index; comparing such a key decodes it.
//! A packed key is never 0 and never has bit 63 set.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

pub const ESCAPE: u64 = 1 << 63;
const MAX_PACKED: usize = 10;

static CODE: [u8; 256] = build_code();
static BYTE: [u8; 64] = build_byte();

const fn build_code() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut i = 0;
    while i < 10 {
        table[b'0' as usize + i] = 1 + i as u8;
        i += 1;
    }
    i = 0;
    while i < 26 {
        table[b'A' as usize + i] = 11 + i as u8;
        i += 1;
    }
    table[b'_' as usize] = 37;
    i = 0;
    while i < 26 {
        table[b'a' as usize + i] = 38 + i as u8;
        i += 1;
    }
    table
}

const fn build_byte() -> [u8; 64] {
    let code = build_code();
    let mut table = [0u8; 64];
    let mut b = 0;
    while b < 256 {
        if code[b] != 0 {
            table[code[b] as usize] = b as u8;
        }
        b += 1;
    }
    table
}

/// The packed key of `bytes`, if it can be packed.
#[inline]
pub fn pack(bytes: &[u8]) -> Option<u64> {
    let n = bytes.len();
    if n == 0 || n > MAX_PACKED {
        return None;
    }
    let mut acc: u64 = 0;
    for &b in bytes {
        let c = CODE[b as usize];
        if c == 0 {
            return None;
        }
        acc = (acc << 6) | c as u64;
    }
    Some(acc << (6 * (MAX_PACKED - n)))
}

struct Interner {
    index: HashMap<Box<[u8]>, u32>,
    bytes: Vec<Box<[u8]>>,
}

fn interner() -> &'static Mutex<Interner> {
    static INTERNER: OnceLock<Mutex<Interner>> = OnceLock::new();
    INTERNER.get_or_init(|| {
        Mutex::new(Interner {
            index: HashMap::new(),
            bytes: Vec::new(),
        })
    })
}

/// The key of a non-empty ID.
#[inline]
pub fn key(bytes: &[u8]) -> u64 {
    match pack(bytes) {
        Some(k) => k,
        None => intern(bytes),
    }
}

#[cold]
fn intern(bytes: &[u8]) -> u64 {
    let mut guard = interner().lock().unwrap();
    if let Some(&i) = guard.index.get(bytes) {
        return ESCAPE | i as u64;
    }
    let i = guard.bytes.len() as u32;
    let boxed: Box<[u8]> = bytes.into();
    guard.bytes.push(boxed.clone());
    guard.index.insert(boxed, i);
    ESCAPE | i as u64
}

/// Appends the ID's bytes to `out`.
pub fn bytes_into(key: u64, out: &mut Vec<u8>) {
    if key & ESCAPE != 0 {
        let guard = interner().lock().unwrap();
        out.extend_from_slice(&guard.bytes[(key & !ESCAPE) as usize]);
    } else {
        let mut shift = 54i32;
        while shift >= 0 {
            let c = ((key >> shift) & 63) as usize;
            if c == 0 {
                break;
            }
            out.push(BYTE[c]);
            shift -= 6;
        }
    }
}

pub fn to_bytes(key: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    bytes_into(key, &mut out);
    out
}

/// Byte-wise order of the two IDs.
#[inline]
pub fn cmp(a: u64, b: u64) -> Ordering {
    if (a | b) & ESCAPE == 0 {
        a.cmp(&b)
    } else {
        cmp_slow(a, b)
    }
}

#[cold]
fn cmp_slow(a: u64, b: u64) -> Ordering {
    to_bytes(a).cmp(&to_bytes(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn round_trip_and_order() {
        let alphabet: &[u8] = b"019AZ_az u\t\r\"x\xc3\xa9-";
        let mut s = 0x1234_5678_9abc_def1u64;
        let mut ids: Vec<Vec<u8>> = vec![b"u1".to_vec(), b"u10".to_vec(), b"U1".to_vec(), b"u1 ".to_vec()];
        for _ in 0..3000 {
            let len = 1 + (rng(&mut s) % 14) as usize;
            let pure = rng(&mut s) % 3 != 0;
            let id: Vec<u8> = (0..len)
                .map(|_| {
                    let pick = if pure { rng(&mut s) % 7 } else { rng(&mut s) % alphabet.len() as u64 };
                    alphabet[pick as usize]
                })
                .collect();
            ids.push(id);
        }
        let keys: Vec<u64> = ids.iter().map(|id| key(id)).collect();
        for (id, &k) in ids.iter().zip(&keys) {
            assert_ne!(k, 0);
            assert_eq!(&to_bytes(k), id);
        }
        for i in 0..ids.len() {
            for j in (i..ids.len()).step_by(37) {
                assert_eq!(cmp(keys[i], keys[j]), ids[i].cmp(&ids[j]), "{:?} {:?}", ids[i], ids[j]);
                assert_eq!(keys[i] == keys[j], ids[i] == ids[j]);
            }
        }
    }

    #[test]
    fn prefix_sorts_first() {
        assert_eq!(cmp(key(b"u1"), key(b"u1x")), Ordering::Less);
        assert_eq!(cmp(key(b"U9"), key(b"u1")), Ordering::Less);
        assert_eq!(cmp(key(b"A\tB"), key(b"A")), Ordering::Greater);
        assert_eq!(cmp(key(b"A\tB"), key(b"u4")), Ordering::Less);
        assert_eq!(pack(b"abcdefghij").is_some(), true);
        assert_eq!(pack(b"abcdefghijk"), None);
    }
}
