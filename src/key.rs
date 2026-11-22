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
    #[cfg(all(target_arch = "x86_64", target_feature = "bmi2", target_feature = "sse4.1"))]
    if (bytes.as_ptr() as usize & 4095) <= 4096 - 16 {
        // SAFETY: the 16 bytes read stay inside the page of bytes[0], which is mapped.
        return unsafe { pack_simd(bytes.as_ptr(), n) };
    }
    pack_scalar(bytes)
}

#[inline]
fn pack_scalar(bytes: &[u8]) -> Option<u64> {
    let n = bytes.len();
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

/// pack() on 16 bytes at once: codes by range (SSE), then the 6-bit fields
/// gathered with pext, first byte highest. Only the first n bytes count.
#[cfg(all(target_arch = "x86_64", target_feature = "bmi2", target_feature = "sse4.1"))]
#[inline(always)]
unsafe fn pack_simd(ptr: *const u8, n: usize) -> Option<u64> {
    use std::arch::x86_64::*;
    let c = _mm_loadu_si128(ptr as *const __m128i);
    let in_range = |lo: i8, hi: i8| _mm_and_si128(_mm_cmpgt_epi8(c, _mm_set1_epi8(lo - 1)), _mm_cmplt_epi8(c, _mm_set1_epi8(hi + 1)));
    let digit = in_range(b'0' as i8, b'9' as i8);
    let upper = in_range(b'A' as i8, b'Z' as i8);
    let under = _mm_cmpeq_epi8(c, _mm_set1_epi8(b'_' as i8));
    let lower = in_range(b'a' as i8, b'z' as i8);
    // code = byte - offset: '0'..'9' -> 1..10, 'A'..'Z' -> 11..36, '_' -> 37, 'a'..'z' -> 38..63
    let offset = _mm_or_si128(
        _mm_or_si128(_mm_and_si128(digit, _mm_set1_epi8(0x2f)), _mm_and_si128(upper, _mm_set1_epi8(0x36))),
        _mm_or_si128(_mm_and_si128(under, _mm_set1_epi8(0x3a)), _mm_and_si128(lower, _mm_set1_epi8(0x3b))),
    );
    let valid = _mm_or_si128(_mm_or_si128(digit, upper), _mm_or_si128(under, lower));
    let iota = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let inside = _mm_cmpgt_epi8(_mm_set1_epi8(n as i8), iota);
    if _mm_movemask_epi8(_mm_andnot_si128(valid, inside)) != 0 {
        return None;
    }
    let codes = _mm_and_si128(_mm_sub_epi8(c, offset), inside);
    let lo = _mm_cvtsi128_si64(codes) as u64; // bytes 0-7
    let hi = _mm_extract_epi64::<1>(codes) as u64; // bytes 8-15 (only 8 and 9 can be set)
    let head = _pext_u64(lo.swap_bytes(), 0x3f3f_3f3f_3f3f_3f3f);
    let tail = _pext_u64(((hi & 0xff) << 8) | ((hi >> 8) & 0xff), 0x3f3f);
    Some((head << 12) | tail)
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
    fn simd_pack_matches_scalar() {
        // A buffer whose IDs start everywhere, including the last bytes of pages.
        let mut s = 0x0123_4567_89ab_cdefu64;
        let alphabet: &[u8] = &[b'0', b'9', b'A', b'Z', b'_', b'a', b'z', b'1', b'9', b'b', b'q', 0x00, 0x7f, 0x80, 0xff, b'/', b':', b'@', b'[', b'`', b'{', b' ', b'-'];
        let mut buf = vec![0u8; 3 * 4096];
        for b in buf.iter_mut() {
            *b = alphabet[(rng(&mut s) % alphabet.len() as u64) as usize];
        }
        for _ in 0..200_000 {
            let start = (rng(&mut s) % (buf.len() as u64 - 16)) as usize;
            let len = (rng(&mut s) % 13) as usize;
            if rng(&mut s) % 2 == 0 {
                for b in &mut buf[start..start + len] {
                    *b = b"0123456789ABCXYZ_abcxyz"[(rng(&mut s) % 23) as usize];
                }
            }
            let id = &buf[start..start + len];
            let reference = if id.is_empty() || id.len() > MAX_PACKED { None } else { pack_scalar(id) };
            assert_eq!(pack(id), reference, "{:?}", id);
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
