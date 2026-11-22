//! Exact 64-bit keys for ID strings, per kind of ID.
//!
//! Every ID column has a usual first letter (`e` for event_id, `u` for user
//! IDs, `k` for click_id). An ID of 2 to 9 bytes that starts with its column's
//! letter and has no NUL byte is "raw": its key is the bytes after the letter,
//! little-endian and zero-padded, so the low byte (the second character) is
//! never zero. Reading one is a single 8-byte load and a mask. Every other
//! non-empty ID is interned and gets `index << 8`, whose low byte is zero.
//!
//! Keys of one column are equal exactly when the IDs are. Raw keys of a column
//! share the first letter, so comparing their big-endian bytes compares the
//! IDs byte by byte; anything involving an interned key compares the strings.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

/// Mask keeping the low `n` bytes (n = 0..=8).
pub const LENMASK: [u64; 9] = [
    0,
    0xFF,
    0xFFFF,
    0xFF_FFFF,
    0xFFFF_FFFF,
    0xFF_FFFF_FFFF,
    0xFFFF_FFFF_FFFF,
    0xFF_FFFF_FFFF_FFFF,
    u64::MAX,
];

#[inline(always)]
pub fn is_raw(k: u64) -> bool {
    k & 0xFF != 0
}

/// Raw key of `s` if it is a raw ID of the column whose letter is `pfx`.
#[inline(always)]
pub fn raw(s: &[u8], pfx: u8) -> Option<u64> {
    let n = s.len();
    if !(2..=9).contains(&n) || s[0] != pfx {
        return None;
    }
    let mut b = [0u8; 8];
    b[..n - 1].copy_from_slice(&s[1..]);
    let v = u64::from_le_bytes(b);
    if has_zero(v, n - 1) {
        return None;
    }
    Some(v)
}

/// Any zero byte among the low `n` bytes of `v` (n = 1..=8).
#[inline(always)]
pub fn has_zero(v: u64, n: usize) -> bool {
    let w = v | !LENMASK[n];
    (w.wrapping_sub(0x0101_0101_0101_0101) & !w & 0x8080_8080_8080_8080) != 0
}

static TABLE: Mutex<Option<HashMap<Vec<u8>, u64>>> = Mutex::new(None);
static FROZEN: RwLock<Vec<Vec<u8>>> = RwLock::new(Vec::new());

/// The key of a non-empty ID of the column whose letter is `pfx`.
#[inline(always)]
pub fn key(s: &[u8], pfx: u8) -> u64 {
    match raw(s, pfx) {
        Some(k) => k,
        None => {
            if s.len() == 33 && s[0] == pfx {
                if let Some(v) = hex128(&s[1..]) {
                    return intern_hex(v);
                }
            }
            intern(s)
        }
    }
}

// ---- Long IDs: the column letter plus exactly 32 lowercase hex digits ----
//
// Scorer v5 gives one in eight IDs this form. Such an ID is exactly a 128-bit
// number (fixed width, lowercase), so it is interned by value in one of 256
// independently locked shards instead of by string under one global lock.
// Keys: bit 63 set, low byte zero (never raw), shard in bits 8..16, index in
// the shard above that. Two such IDs of one column compare like their numbers,
// because fixed-width lowercase hex sorts like the value.

const HEX_TAG: u64 = 1 << 63;
const HEX_SHARDS: usize = 256;

#[derive(Default)]
struct Fx(u64);
impl std::hash::Hasher for Fx {
    #[inline(always)]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline(always)]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x51_7cc1_b727_220a_95);
        }
    }
    #[inline(always)]
    fn write_u128(&mut self, v: u128) {
        self.0 = ((v as u64) ^ ((v >> 64) as u64).rotate_left(29)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
}
type FxBuild = std::hash::BuildHasherDefault<Fx>;

struct HexShard {
    map: HashMap<u128, u64, FxBuild>,
    vals: Vec<u128>,
}

static HEX: [Mutex<Option<HexShard>>; HEX_SHARDS] = [const { Mutex::new(None) }; HEX_SHARDS];

/// Value of exactly 32 lowercase hex digits.
#[inline(always)]
fn hex128(s: &[u8]) -> Option<u128> {
    let mut v: u128 = 0;
    for &c in &s[..32] {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => return None,
        };
        v = v << 4 | d as u128;
    }
    Some(v)
}

#[inline]
fn intern_hex(v: u128) -> u64 {
    let shard = ((v as u64) ^ ((v >> 64) as u64)) as usize & (HEX_SHARDS - 1);
    let mut guard = HEX[shard].lock().unwrap();
    let sh = guard.get_or_insert_with(|| HexShard { map: HashMap::with_hasher(FxBuild::default()), vals: Vec::new() });
    if let Some(&k) = sh.map.get(&v) {
        return k;
    }
    let k = HEX_TAG | (sh.vals.len() as u64) << 16 | (shard as u64) << 8;
    sh.vals.push(v);
    sh.map.insert(v, k);
    k
}

#[inline(always)]
fn is_hex(k: u64) -> bool {
    k & HEX_TAG != 0 && !is_raw(k)
}

fn hex_value(k: u64) -> u128 {
    let shard = ((k >> 8) & 0xFF) as usize;
    let idx = ((k & !HEX_TAG) >> 16) as usize;
    HEX[shard].lock().unwrap().as_ref().unwrap().vals[idx]
}

#[cold]
pub fn intern(s: &[u8]) -> u64 {
    let mut guard = TABLE.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(&k) = map.get(s) {
        return k;
    }
    let mut frozen = FROZEN.write().unwrap();
    let k = (frozen.len() as u64 + 1) << 8;
    frozen.push(s.to_vec());
    map.insert(s.to_vec(), k);
    k
}

/// Append the ID behind `k` (a key of the column whose letter is `pfx`).
#[inline]
pub fn bytes(k: u64, pfx: u8, out: &mut Vec<u8>) {
    if is_raw(k) {
        out.push(pfx);
        let b = k.to_le_bytes();
        let n = 8 - (k.leading_zeros() as usize / 8);
        out.extend_from_slice(&b[..n]);
    } else if is_hex(k) {
        out.push(pfx);
        let v = hex_value(k);
        for i in (0..32).rev() {
            out.push(b"0123456789abcdef"[((v >> (4 * i)) & 15) as usize]);
        }
    } else {
        out.extend_from_slice(&FROZEN.read().unwrap()[((k >> 8) - 1) as usize]);
    }
}

/// Byte-wise comparison of the IDs behind two keys of one column.
#[inline]
pub fn cmp(a: u64, b: u64, pfx: u8) -> Ordering {
    if is_raw(a) && is_raw(b) {
        return a.swap_bytes().cmp(&b.swap_bytes());
    }
    if is_hex(a) && is_hex(b) {
        return if a == b { Ordering::Equal } else { hex_value(a).cmp(&hex_value(b)) };
    }
    cmp_slow(a, b, pfx)
}

#[cold]
fn cmp_slow(a: u64, b: u64, pfx: u8) -> Ordering {
    let (mut x, mut y) = (Vec::new(), Vec::new());
    bytes(a, pfx, &mut x);
    bytes(b, pfx, &mut y);
    x.cmp(&y)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn order_and_roundtrip() {
        let words: Vec<&[u8]> = vec![
            b"u1", b"U1", b"u", b"uz", b"u10", b"_", b"a", b"0", b"zzzzzzzzzz", b" u1", b"u1 ",
            b"u1\r", b"abcdefghijk", b"user_id_a", b"user_id_b\t", b"p\xc3\xa9", b"u\xc3\xa9",
            b"uzzzzzzzz", b"u00000000", b"u0000000", b"u~~~~~~~~", b"u\x7f", b"ua\x00b", b"u\x00",
            b"uab\x00", b"u12345678", b"u123456789", b"U1ABCDEFG", b" ", b"  ", b"u\xff\xff\xff\xff\xff\xff\xff",
            b"u0123456789abcdef0123456789abcdef", b"uffffffffffffffffffffffffffffffff", b"u00000000000000000000000000000000",
            b"u0123456789abcdef0123456789abcdeF", b"u0123456789abcdef0123456789abcde", b"u0123456789abcdef0123456789abcdef0",
            b"e0123456789abcdef0123456789abcdef", b"ua23456789abcdef0123456789abcdef0", b"u9fffffffffffffffffffffffffffffff",
            b"k0123456789abcdef0123456789abcdef", b"u0123456789abcdeg0123456789abcdef",
        ];
        for pfx in [b'u', b'e', b'k'] {
            for a in &words {
                let mut out = Vec::new();
                bytes(key(a, pfx), pfx, &mut out);
                assert_eq!(&out[..], *a, "roundtrip {:?}", String::from_utf8_lossy(a));
                for b in &words {
                    assert_eq!(cmp(key(a, pfx), key(b, pfx), pfx), a.cmp(b), "{:?} {:?}", a, b);
                    assert_eq!(key(a, pfx) == key(b, pfx), a == b);
                }
            }
        }
    }
}
