//! Front-end prototype for the v2 pipeline. TIMING ONLY: it shows what one pass over
//! the event lines can cost when it does, per line and in this order,
//!
//!   stage 1  64-byte blocks -> separator positions (flattened), quote flags per field
//!            start / field end (two bit streams), per-newline index and quote count
//!   stage 2  per line: 7 fields from one vector load, quotes stripped without branches,
//!            `prefix + decimal` IDs as dense numbers, SSE F1 timestamps, validation,
//!            results appended to a small batch (+ prefetch of what stage 3 will touch)
//!   stage 3  per batch: test-and-set in a dedup bitmap indexed by event number, person
//!            from a direct `names[user]` array, 16-byte record write-combined into
//!            person-range partitions
//!
//! against today's parse -> scatter by event_id -> dedup tables -> name lookups -> scatter
//! by person. Measured on a 2.0 GHz Xeon vCPU, one thread, 3M synthetic event lines:
//!
//!   level 0  stage 1 only                           18 ns/line
//!   level 1  + line assembly and quotes             23
//!   level 2  + the two IDs                          33
//!   level 3  + ts and ingest_ts                     54   (62 with src/ts.rs: SLOW_TS=1)
//!   level 4  + validation, batch, bitmap, names     64
//!   level 5  + scatter                              75
//!
//! The same scope in the current pipeline (parse 90 + dedup 32 + names 7) is 129 ns/line.
//! On that dataset the counters below (good, bad, exotic, repeats) equal those computed
//! with reference.py's parse_line / parse_ts.
//!
//! NOT a reference for correctness. Known shortcuts: `names` is a fake table, exotic
//! IDs are only counted, F2 goes to the slow parser, a line longer than SUB is skipped,
//! every file's header is parsed like a data line, the price loop is naive, nothing
//! is deduplicated (the bitmap only detects), nothing is sessionized.
//!
//! Adapted to the real IDs (prefix + base 36 of a 40-bit pseudo-random number): base 36
//! decoder, a 2^28-2^29-bit bitmap indexed by a hash of the event value, names as a filter
//! plus a 16-byte-slot table with 40% of rows renamed, partitions by low bits of the person.
//! p20 on an i7-12700K, one thread: levels 0-5 = 8.8 / 12.8 / 18.9 / 29.0 / 45.4 / 54.1 ns/line.
//!
//! Usage: cargo run --release -- DATA_DIR LEVEL [REPS] [MAX_MB]
#![allow(dead_code)]

#[path = "../../src/ts.rs"]
mod ts;
#[path = "../../src/csv.rs"]
mod csv;
#[path = "../../src/input.rs"]
mod input;

use std::arch::x86_64::*;
use std::time::Instant;

/// Bytes of padding before and after the data, so loads around a field never leave the buffer.
const PAD_FRONT: usize = 64;
const PAD_BACK: usize = 320;
/// Bytes tokenized by stage 1 before stage 2 runs: the index arrays stay in L1/L2.
const SUB: usize = 16 << 10;

static MONTH_START: [i32; 3600] = build_month_start();

const fn build_month_start() -> [i32; 3600] {
    let mut table = [0i32; 3600];
    let mut i = 0;
    while i < 3600 {
        table[i] = ts::days_from_civil(1900 + (i / 12) as i64, (i % 12) as i64 + 1, 1) as i32;
        i += 1;
    }
    table
}

#[inline(always)]
unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
    let lo = _mm256_loadu_si256(p as *const __m256i);
    let hi = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let m = |v: __m256i, c: u8| -> u64 { _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c as i8))) as u32 as u64 };
    (m(lo, b'\n') | m(hi, b'\n') << 32, m(lo, b',') | m(hi, b',') << 32, m(lo, b'"') | m(hi, b'"') << 32)
}

/// 8 ASCII digits (first digit in the low byte) as a number; None if a byte is not 0-9.
#[inline(always)]
fn digits8(word: u64) -> Option<u64> {
    let v = word.wrapping_sub(0x3030_3030_3030_3030);
    if (v | v.wrapping_add(0x7676_7676_7676_7676)) & 0x8080_8080_8080_8080 != 0 {
        return None;
    }
    let v = (v.wrapping_mul(10) + (v >> 8)) & 0x00ff_00ff_00ff_00ff;
    let v = (v.wrapping_mul(100) + (v >> 16)) & 0x0000_ffff_0000_ffff;
    Some((v.wrapping_mul(10_000) + (v >> 32)) & 0xffff_ffff)
}

/// The shape of an ID column: `prefix` (1 to 4 bytes) + canonical lowercase base 36.
#[derive(Clone, Copy)]
struct Scheme {
    word: u32,
    mask: u32,
    len: usize,
}

impl Scheme {
    /// The common prefix of the sample IDs (1 to 4 bytes, shorter than the shortest).
    fn of(samples: &[Vec<u8>]) -> Self {
        let first = &samples[0];
        let shortest = samples.iter().map(Vec::len).min().unwrap_or(1);
        let mut len = 0;
        while len < 4 && len + 1 < shortest && samples.iter().all(|x| x[len] == first[len]) {
            len += 1;
        }
        let len = len.max(1);
        let mut bytes = [0u8; 4];
        bytes[..len].copy_from_slice(&first[..len]);
        let mask = if len == 4 { !0 } else { (1u32 << (8 * len)) - 1 };
        Self { word: u32::from_le_bytes(bytes) & mask, mask, len }
    }
}

/// `prefix` + canonical lowercase base 36 of 1 to 8 chars -> its value (< 36^8 < 2^42).
/// None = not of that shape ("exotic"). SSE: digit values of the last 8 bytes of the
/// field, then pairs (x36) and quads (x1296) with maddubs / madd.
#[inline(always)]
unsafe fn b36_id(b: *const u8, s: usize, e: usize, scheme: Scheme) -> Option<u64> {
    let nd = e.wrapping_sub(s).wrapping_sub(scheme.len);
    let head = (b.add(s) as *const u32).read_unaligned();
    if nd.wrapping_sub(1) > 7 || head & scheme.mask != scheme.word {
        return None;
    }
    if *b.add(s + scheme.len) == b'0' && nd > 1 {
        return None;
    }
    let w = (b.add(e - 8) as *const u64).read_unaligned();
    let keep = !0u64 << (8 * (8 - nd));
    let w = (w & keep) | (0x3030_3030_3030_3030 & !keep); // leading bytes forced to '0'
    let v = _mm_cvtsi64_si128(w as i64);
    let digit = _mm_and_si128(_mm_cmpgt_epi8(v, _mm_set1_epi8(b'0' as i8 - 1)), _mm_cmplt_epi8(v, _mm_set1_epi8(b'9' as i8 + 1)));
    let lower = _mm_and_si128(_mm_cmpgt_epi8(v, _mm_set1_epi8(b'a' as i8 - 1)), _mm_cmplt_epi8(v, _mm_set1_epi8(b'z' as i8 + 1)));
    if _mm_movemask_epi8(_mm_or_si128(digit, lower)) & 0xff != 0xff {
        return None;
    }
    let offset = _mm_or_si128(_mm_and_si128(digit, _mm_set1_epi8(48)), _mm_and_si128(lower, _mm_set1_epi8(87)));
    let d = _mm_sub_epi8(v, offset);
    let pairs = _mm_maddubs_epi16(d, _mm_setr_epi8(36, 1, 36, 1, 36, 1, 36, 1, 0, 0, 0, 0, 0, 0, 0, 0));
    let quads = _mm_madd_epi16(pairs, _mm_setr_epi16(1296, 1, 1296, 1, 0, 0, 0, 0));
    let hi = _mm_cvtsi128_si32(quads) as u32 as u64;
    let lo = _mm_extract_epi32::<1>(quads) as u32 as u64;
    Some(hi * 1_679_616 + lo)
}

#[inline(always)]
fn mix42(v: u64) -> u64 {
    v.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// F1 `YYYY-MM-DDTHH:MM:SSZ` (20 bytes at p) as UTC milliseconds: one 16-byte load
/// checks every separator and digit and yields day, hour, minute, second and month.
#[inline(always)]
unsafe fn f1_fast(p: *const u8) -> Option<i64> {
    let v = _mm_loadu_si128(p.add(4) as *const __m128i); // "-MM-DDTHH:MM:SSZ"
    let shape = _mm_setr_epi8(b'-' as i8, 48, 48, b'-' as i8, 48, 48, b'T' as i8, 48, 48, b':' as i8, 48, 48, b':' as i8, 48, 48, b'Z' as i8);
    let limit = _mm_setr_epi8(0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0, 9, 9, 0);
    let t = _mm_sub_epi8(v, shape); // 0 at separators, the digit value at digits
    let shape_ok = _mm_cmpeq_epi8(_mm_max_epu8(t, limit), limit);
    let digits = _mm_shuffle_epi8(t, _mm_setr_epi8(4, 5, 7, 8, 10, 11, 13, 14, 1, 2, -1, -1, -1, -1, -1, -1));
    let pairs = _mm_maddubs_epi16(digits, _mm_setr_epi8(10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 0, 0, 0, 0, 0, 0)); // DD HH MM SS MO
    let over = _mm_cmpgt_epi16(pairs, _mm_setr_epi16(31, 23, 59, 59, 12, 0, 0, 0));
    let y = (p as *const u32).read_unaligned().wrapping_sub(0x3030_3030);
    let year_bad = (y | y.wrapping_add(0x7676_7676)) & 0x8080_8080;
    let y = (y.wrapping_mul(10).wrapping_add(y >> 8)) & 0x00ff_00ff;
    let year = (y & 0xff) * 100 + (y >> 16);
    let day = _mm_extract_epi16(pairs, 0) as u32;
    let hour = _mm_extract_epi16(pairs, 1) as u32;
    let minute = _mm_extract_epi16(pairs, 2) as u32;
    let second = _mm_extract_epi16(pairs, 3) as u32;
    let month = _mm_extract_epi16(pairs, 4) as u32;
    let index = (year as usize).wrapping_sub(1900).wrapping_mul(12).wrapping_add((month as usize).wrapping_sub(1));
    if (_mm_movemask_epi8(shape_ok) != 0xffff) | (_mm_movemask_epi8(over) != 0) | (year_bad != 0) | (month == 0) | (day == 0) | (index >= 3598) {
        return ts::parse(std::slice::from_raw_parts(p, 20)); // rare: the exact parser decides
    }
    let first = *MONTH_START.get_unchecked(index);
    let next = *MONTH_START.get_unchecked(index + 1);
    if day as i32 > next - first {
        return None;
    }
    Some(((first as i64 + day as i64 - 1) * 86_400 + (hour * 3600 + minute * 60 + second) as i64) * 1000)
}

#[inline(always)]
unsafe fn ts_fast(p: *const u8, len: usize) -> Option<i64> {
    if len == 20 {
        return f1_fast(p);
    }
    if len == 13 {
        let hi = (p as *const u64).read_unaligned();
        let lo = (p.add(5) as *const u64).read_unaligned();
        return match (digits8(hi), digits8(lo)) {
            (Some(h), Some(l)) => Some((h * 100_000 + l % 100_000) as i64),
            _ => None,
        };
    }
    ts::parse(std::slice::from_raw_parts(p, len))
}

/// 16-byte records write-combined into partitions: one 64-byte staging line per
/// partition, flushed with non-temporal stores into 4 KiB blocks carved from one arena.
struct Scatter {
    stage: Vec<[u64; 8]>,
    fill: Vec<u8>,
    cur: Vec<*mut u64>,
    left: Vec<u32>,
    arena: *mut u64,
    used: usize,
}

impl Scatter {
    fn new(parts: usize, records: usize) -> Self {
        let words = (records * 2 + parts * 512 * 2 + 4096).next_multiple_of(512);
        let layout = std::alloc::Layout::from_size_align(words * 8, 2 << 20).unwrap();
        let arena = unsafe { std::alloc::alloc(layout) } as *mut u64;
        assert!(!arena.is_null());
        #[cfg(target_os = "linux")]
        unsafe {
            extern "C" {
                fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
            }
            madvise(arena as *mut u8, words * 8, 14); // MADV_HUGEPAGE
        }
        Self { stage: vec![[0; 8]; parts], fill: vec![0; parts], cur: vec![std::ptr::null_mut(); parts], left: vec![0; parts], arena, used: 0 }
    }

    #[inline(always)]
    unsafe fn push(&mut self, p: usize, a: u64, b: u64) {
        let f = *self.fill.get_unchecked(p) as usize;
        let line = self.stage.get_unchecked_mut(p);
        *line.get_unchecked_mut(f) = a;
        *line.get_unchecked_mut(f + 1) = b;
        if f == 6 {
            if *self.left.get_unchecked(p) == 0 {
                *self.cur.get_unchecked_mut(p) = self.arena.add(self.used);
                self.used += 512;
                *self.left.get_unchecked_mut(p) = 64;
            }
            let d = *self.cur.get_unchecked(p) as *mut __m256i;
            let s = line.as_ptr() as *const __m256i;
            _mm256_stream_si256(d, _mm256_loadu_si256(s));
            _mm256_stream_si256(d.add(1), _mm256_loadu_si256(s.add(1)));
            *self.cur.get_unchecked_mut(p) = (d as *mut u64).add(8);
            *self.left.get_unchecked_mut(p) -= 1;
            *self.fill.get_unchecked_mut(p) = 0;
        } else {
            *self.fill.get_unchecked_mut(p) = (f + 2) as u8;
        }
    }
}

/// Appends bit fields to a stream of u64 words.
struct BitWriter {
    p: *mut u64,
    acc: u64,
    fill: u32,
}

impl BitWriter {
    /// `n` <= 64 bits; `bits` has nothing at or above bit n.
    #[inline(always)]
    unsafe fn put(&mut self, bits: u64, n: u32) {
        self.acc |= bits << self.fill;
        let total = self.fill + n;
        if total >= 64 {
            *self.p = self.acc;
            self.p = self.p.add(1);
            self.acc = if self.fill == 0 { 0 } else { bits >> (64 - self.fill) };
            self.fill = total - 64;
        } else {
            self.fill = total;
        }
    }

    #[inline(always)]
    unsafe fn flush(&mut self) {
        *self.p = self.acc;
        *self.p.add(1) = 0;
    }
}

/// Bits i..i+7 of a bit stream.
#[inline(always)]
unsafe fn read7(stream: *const u64, i: usize) -> u32 {
    (((stream as *const u8).add(i >> 3) as *const u64).read_unaligned() >> (i & 7)) as u32 & 0x7f
}

#[derive(Default)]
struct Counts {
    lines: u64,
    good: u64,
    bad: u64,
    exotic: u64,
    malformed: u64,
    repeats: u64,
    sum: u64,
}

struct Shape {
    window_start: i64,
    window_end: i64,
    base: i64,
    event: Scheme,
    user: Scheme,
}

#[target_feature(enable = "avx2,bmi1,bmi2,popcnt,lzcnt")]
unsafe fn run(buf: &[u8], data_len: usize, level: u32, shape: &Shape, names: &Names, bitmap: &mut [u64], out: &mut Scatter) -> Counts {
    let b = buf.as_ptr().add(PAD_FRONT);
    let mut o = Counts::default();
    let fast_ts = std::env::var_os("SLOW_TS").is_none();
    // stage 1 output
    let mut pos = vec![0u32; SUB + 1024]; // separator positions; slot 0 is the line start - 1
    let mut nl_idx = vec![0u32; SUB / 2 + 1024]; // per newline: its index in pos
    let mut nl_quotes = vec![0u32; SUB / 2 + 1024]; // per newline: quotes before it
    let mut q_start = vec![0u64; SUB / 64 + 64]; // bit i: the field after pos[i] starts with a quote
    let mut q_end = vec![0u64; SUB / 64 + 64]; // bit i: the byte before pos[i] is a quote
    // stage 2 output (the batch)
    let cap = SUB / 8 + 64;
    let (mut b_ts, mut b_event, mut b_user, mut b_aux) = (vec![0u64; cap], vec![0u64; cap], vec![0u64; cap], vec![0u32; cap]);
    let parts = out.stage.len();
    let bit_shift = 64 - (bitmap.len() * 64).trailing_zeros(); // bitmap bits: a power of two
    let part_mask = parts as u64 - 1; // parts: a power of two
    let lane_bit = _mm256_setr_epi32(1, 2, 4, 8, 16, 32, 64, 128);
    let mut line_start = 0usize; // always the first byte of a line
    while line_start < data_len {
        // ---- stage 1: whole blocks from the one holding line_start
        let at = line_start & !63;
        let end = (at + SUB).min((data_len + 63) & !63);
        let pp = pos.as_mut_ptr();
        *pp = ((line_start - at) as u32).wrapping_sub(1);
        let (mut np, mut nn, mut quotes) = (1usize, 0usize, 0u32);
        let mut starts_w = BitWriter { p: q_start.as_mut_ptr(), acc: 0, fill: 0 };
        let mut ends_w = BitWriter { p: q_end.as_mut_ptr(), acc: 0, fill: 0 };
        ends_w.put(0, 1); // keeps the q_end index equal to the pos index
        let mut live = !0u64 << (line_start & 63); // bytes before the line start do not count
        let mut first_field = 1u64 << (line_start & 63);
        let (mut carry_sep, mut carry_quote) = (0u64, 0u64);
        let mut block = at;
        while block < end {
            let (nl, comma, quote) = masks(b.add(block));
            let (nl, quote, sep) = (nl & live, quote & live, (nl | comma) & live);
            live = !0;
            let rel = (block - at) as u32;
            let count = sep.count_ones() as usize;
            {
                // 8 positions written unconditionally: no branch for the usual block
                let dst = pp.add(np);
                let mut bits = sep;
                macro_rules! put {
                    ($i:expr) => {
                        *dst.add($i) = rel + bits.trailing_zeros();
                        bits &= bits.wrapping_sub(1);
                    };
                }
                put!(0);
                put!(1);
                put!(2);
                put!(3);
                put!(4);
                put!(5);
                put!(6);
                put!(7);
                let mut i = 8;
                while bits != 0 {
                    put!(i);
                    i += 1;
                }
            }
            let starts = (sep << 1) | carry_sep | first_field;
            first_field = 0;
            carry_sep = sep >> 63;
            starts_w.put(_pext_u64(quote, starts), starts.count_ones());
            ends_w.put(_pext_u64((quote << 1) | carry_quote, sep), count as u32);
            carry_quote = quote >> 63;
            // newlines: two written unconditionally
            let mut rest = nl;
            let newlines = nl.count_ones() as usize;
            let mut k = 0;
            loop {
                let below = (rest & rest.wrapping_neg()).wrapping_sub(1);
                *nl_idx.get_unchecked_mut(nn + k) = np as u32 + (sep & below).count_ones();
                *nl_quotes.get_unchecked_mut(nn + k) = quotes + (quote & below).count_ones();
                rest &= rest.wrapping_sub(1);
                k += 1;
                if k >= 2 && rest == 0 {
                    break;
                }
            }
            nn += newlines;
            np += count;
            quotes += quote.count_ones();
            block += 64;
        }
        starts_w.flush();
        ends_w.flush();
        if nn == 0 {
            line_start = end; // prototype: a line longer than SUB
            continue;
        }
        o.lines += nn as u64;
        line_start = at + *pp.add(*nl_idx.get_unchecked(nn - 1) as usize) as usize + 1;
        if level == 0 {
            o.sum += np as u64;
            continue;
        }
        // ---- stage 2: every line that ends in these blocks
        let bb = b.add(at);
        let (mut next_j, mut quotes_before, mut n) = (1usize, 0u32, 0usize);
        for li in 0..nn {
            let j_end = *nl_idx.get_unchecked(li) as usize;
            let line_quotes = *nl_quotes.get_unchecked(li) - quotes_before;
            quotes_before = *nl_quotes.get_unchecked(li);
            let j = next_j;
            next_j = j_end + 1;
            if j_end - j != 6 {
                o.malformed += 1; // not 7 fields
                continue;
            }
            // a field is quoted iff it starts and ends with a quote; the line is well
            // formed iff those are its only quotes
            let quoted = read7(q_start.as_ptr(), j - 1);
            if (quoted != read7(q_end.as_ptr(), j)) | (2 * quoted.count_ones() != line_quotes) {
                o.malformed += 1;
                continue;
            }
            let lanes = _mm256_cmpeq_epi32(_mm256_and_si256(_mm256_set1_epi32(quoted as i32), lane_bit), lane_bit);
            let starts = _mm256_sub_epi32(_mm256_sub_epi32(_mm256_loadu_si256(pp.add(j - 1) as *const __m256i), _mm256_set1_epi32(-1)), lanes);
            let ends = _mm256_add_epi32(_mm256_loadu_si256(pp.add(j) as *const __m256i), lanes);
            let (mut s, mut e) = ([0u32; 8], [0u32; 8]);
            _mm256_storeu_si256(s.as_mut_ptr() as *mut __m256i, starts);
            _mm256_storeu_si256(e.as_mut_ptr() as *mut __m256i, ends);
            let (s, e) = (s.map(|x| x as usize), e.map(|x| x as usize));
            if s[0] == e[0] {
                o.malformed += 1; // empty event_id (counted with the malformed here)
                continue;
            }
            if level == 1 {
                o.sum += (e[0] - s[0] + e[6] - s[6]) as u64;
                continue;
            }
            let event = b36_id(bb, s[0], e[0], shape.event);
            let user = b36_id(bb, s[1], e[1], shape.user);
            if level == 2 {
                o.sum += event.unwrap_or(0) + user.unwrap_or(0);
                continue;
            }
            let (ingest, t) = if fast_ts {
                let ingest = if e[3] - s[3] == 20 {
                    f1_fast(bb.add(s[3])).unwrap_or(i64::MAX)
                } else {
                    ts::ingest(std::slice::from_raw_parts(bb.add(s[3]), e[3] - s[3]))
                };
                (ingest, ts_fast(bb.add(s[2]), e[2] - s[2]))
            } else {
                (ts::ingest(std::slice::from_raw_parts(bb.add(s[3]), e[3] - s[3])), ts::parse(std::slice::from_raw_parts(bb.add(s[2]), e[2] - s[2])))
            };
            if level == 3 {
                o.sum += ingest as u64 + t.unwrap_or(0) as u64;
                continue;
            }
            let type_len = e[4] - s[4];
            let w4 = (bb.add(s[4]) as *const u32).read_unaligned();
            let w8 = (bb.add(s[4]) as *const u64).read_unaligned();
            let view = (type_len == 4) & ((w4 == u32::from_le_bytes(*b"view")) | (w4 == u32::from_le_bytes(*b"cart")));
            let purchase = (type_len == 8) & (w8 == u64::from_le_bytes(*b"purchase"));
            let (Some(event), Some(user)) = (event, user) else {
                o.exotic += 1; // the real pipeline interns these (rare) IDs
                continue;
            };
            let t = t.unwrap_or(i64::MIN);
            let mut ok = (view | purchase) & (e[5] > s[5]) & (t >= shape.window_start) & (t < shape.window_end) & (ingest.saturating_sub(t) <= 86_400_000);
            let mut aux = (((ingest - shape.base) / 1000) as u32) & 0x3fff_ffff;
            if purchase {
                let mut cents = 0u64;
                let mut digits = e[6] > s[6];
                for i in s[6]..e[6] {
                    let d = (*bb.add(i)).wrapping_sub(b'0');
                    digits &= d < 10;
                    cents = (cents * 10 + d as u64).min(1 << 40);
                }
                ok &= digits & (cents >= 1) & (cents <= 10_000_000);
                aux = 1 << 30 | cents as u32;
            }
            *b_ts.get_unchecked_mut(n) = t.wrapping_sub(shape.base) as u64;
            *b_event.get_unchecked_mut(n) = event;
            *b_user.get_unchecked_mut(n) = user;
            *b_aux.get_unchecked_mut(n) = aux | (ok as u32) << 31;
            let eh = (mix42(event) >> bit_shift) as usize;
            _mm_prefetch(bitmap.as_ptr().wrapping_add(eh >> 6) as *const i8, _MM_HINT_T0);
            let uh = mix42(user);
            _mm_prefetch(names.filter.as_ptr().wrapping_add((uh >> names.filter_shift) as usize >> 6) as *const i8, _MM_HINT_T0);
            _mm_prefetch(names.slots.as_ptr().wrapping_add((uh >> names.slot_shift) as usize) as *const i8, _MM_HINT_T0);
            n += 1;
        }
        if level < 4 {
            continue;
        }
        // ---- stage 3: what the prefetches were for
        for i in 0..n {
            let event = *b_event.get_unchecked(i);
            let eh = (mix42(event) >> bit_shift) as usize;
            let word = eh >> 6;
            let bit = 1u64 << (eh & 63);
            let old = *bitmap.get_unchecked(word);
            o.repeats += (old & bit != 0) as u64;
            *bitmap.get_unchecked_mut(word) = old | bit;
            let aux = *b_aux.get_unchecked(i);
            if aux >> 31 == 0 {
                o.bad += 1; // the real pipeline keeps (event, ingest) of these for the dedup
                continue;
            }
            let person = names.get(*b_user.get_unchecked(i));
            o.good += 1;
            if level == 4 {
                o.sum += person + aux as u64;
                continue;
            }
            let m = mix42(person) >> 22; // 42 significant bits
            let part = (m & part_mask) as usize;
            out.push(part, (m >> 10) << 32 | *b_ts.get_unchecked(i), event << 22 | (aux & 0x3f_ffff) as u64);
        }
    }
    o
}

/// A stand-in for the names lookup of v2-R: a filter of renamed users (8 bits per linked
/// ID) and an open-addressing table of 16-byte (key, name) slots for the renamed ones,
/// sized like the real ones at this scale; about 40% of rows hit a renamed user.
struct Names {
    filter: Vec<u64>,
    filter_shift: u32,
    slots: Vec<[u64; 2]>,
    slot_shift: u32,
}

impl Names {
    fn new(linked: usize) -> Self {
        let filter_bits = (linked * 8).next_power_of_two().max(1 << 12);
        let slots = (linked * 2).next_power_of_two().max(1 << 12);
        let mut filter = vec![0u64; filter_bits / 64];
        // "renamed": 2 filter words in 5 are all ones -> 40% of users
        for (i, w) in filter.iter_mut().enumerate() {
            *w = if i % 5 < 2 { !0 } else { 0 };
        }
        let mut st = 0x1234_5678u64;
        let slots: Vec<[u64; 2]> = (0..slots).map(|i| { st ^= st << 13; st ^= st >> 7; st ^= st << 17; [st, i as u64] }).collect();
        Self { filter, filter_shift: 64 - filter_bits.trailing_zeros(), slot_shift: 64 - slots.len().trailing_zeros(), slots }
    }

    #[inline(always)]
    fn get(&self, user: u64) -> u64 {
        let h = mix42(user);
        let b = (h >> self.filter_shift) as usize;
        // SAFETY: b < filter bits
        if unsafe { *self.filter.get_unchecked(b >> 6) } >> (b & 63) & 1 == 0 {
            return user;
        }
        let mut i = (h >> self.slot_shift) as usize;
        loop {
            // SAFETY: i < slots.len()
            let [k, name] = unsafe { *self.slots.get_unchecked(i) };
            if k & 7 != 0 || i + 1 == self.slots.len() {
                return name ^ k ^ user; // "found" (most of the time at the first probe)
            }
            i += 1;
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: frontend-proto DATA_DIR LEVEL(0-5) [REPS=7] [MAX_MB=512]");
        std::process::exit(2);
    }
    let (dir, level) = (&args[1], args[2].parse::<u32>().unwrap());
    let reps: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(7);
    let max_bytes: usize = args.get(4).map(|s| s.parse::<usize>().unwrap()).unwrap_or(512) << 20;
    let mut files: Vec<_> = std::fs::read_dir(format!("{dir}/events")).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|x| x == "csv")).collect();
    files.sort();
    let mut buf = vec![b'\n'; PAD_FRONT];
    let mut samples: Vec<Vec<u8>> = Vec::new(); // the first data lines
    for f in &files {
        if buf.len() >= max_bytes {
            break;
        }
        let data = std::fs::read(f).unwrap();
        if samples.len() < 64 {
            let need = 64 - samples.len();
            samples.extend(data.split(|&c| c == b'\n').skip(1).filter(|l| l.iter().filter(|&&c| c == b',').count() == 6).take(need).map(|l| l.to_vec()));
        }
        buf.extend_from_slice(&data);
        if buf.last() != Some(&b'\n') {
            buf.push(b'\n');
        }
    }
    let data_len = buf.len() - PAD_FRONT;
    buf.extend(std::iter::repeat_n(b'\n', PAD_BACK));
    let manifest = std::fs::read_to_string(format!("{dir}/manifest.json")).unwrap();
    let field = |name: &str| {
        let rest = &manifest[manifest.find(name).unwrap() + name.len()..];
        let value = rest.split('"').nth(2).unwrap();
        ts::parse(value.as_bytes()).unwrap()
    };
    let (window_start, window_end) = (field("window_start"), field("window_end"));
    let column = |k: usize| -> Vec<Vec<u8>> {
        samples
            .iter()
            .map(|l| {
                let f = l.split(|&c| c == b',').nth(k).unwrap();
                let f = f.strip_prefix(b"\"").unwrap_or(f);
                f.strip_suffix(b"\"").unwrap_or(f).to_vec()
            })
            .filter(|f| !f.is_empty())
            .collect()
    };
    let shape = Shape {
        window_start,
        window_end,
        base: window_start - 7 * 86_400_000,
        event: Scheme::of(&column(0)),
        user: Scheme::of(&column(1)),
    };
    let lines_estimate = data_len / 60 + 1024;
    // p20: 1.2M linked IDs for 21.5M event lines
    let names = Names::new(lines_estimate / 18);
    let bitmap_bits = (lines_estimate * 12).next_power_of_two().max(1 << 12);
    eprintln!(
        "schemes: event prefix {} B, user prefix {} B | bitmap 2^{} bits | names filter {} KiB, table {} MiB",
        shape.event.len,
        shape.user.len,
        bitmap_bits.trailing_zeros(),
        names.filter.len() / 128,
        names.slots.len() >> 16
    );
    let mut best = f64::MAX;
    let mut last = Counts::default();
    for _ in 0..reps {
        let mut bitmap = vec![0u64; bitmap_bits / 64];
        let mut out = Scatter::new(1024, lines_estimate);
        let started = Instant::now();
        let counts = if level >= 10 {
            // the current pipeline's tokenizer (src/input.rs) on the same buffer
            let mut o = Counts::default();
            let data = &buf[PAD_FRONT..];
            let mut f = |x: &[&[u8]; 7]| {
                o.good += 1;
                o.sum += (x[0].len() + x[6].len()) as u64;
                if level >= 11 {
                    o.sum += ts::ingest(x[3]) as u64 + ts::parse(x[2]).unwrap_or(0) as u64;
                }
            };
            input::range_lines::<7, _>(data, 0, data_len, data_len, true, &mut f);
            o.lines = o.good;
            o
        } else {
            unsafe { run(&buf, data_len, level, &shape, &names, &mut bitmap, &mut out) }
        };
        best = best.min(started.elapsed().as_secs_f64());
        last = counts;
    }
    println!(
        "level {level}: {:.1} ms, {:.1} ns/line, {:.2} GB/s | lines {} good {} bad {} exotic {} malformed {} repeats {} (checksum {})",
        best * 1e3,
        best * 1e9 / last.lines.max(1) as f64,
        data_len as f64 / best / 1e9,
        last.lines,
        last.good,
        last.bad,
        last.exotic,
        last.malformed,
        last.repeats,
        last.sum
    );
}
