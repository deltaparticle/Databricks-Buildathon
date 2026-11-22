//! Reading the CSV folders: memory-mapped files cut into byte ranges; the thread
//! that parses a range finds its first line itself (the lines that start in the
//! range are its own) and releases the range's pages once parsed. With SS_IO=pread
//! the chunks are line-aligned up front and read into a buffer padded with `\n`.
//! Lines are split into fields with AVX2 when available.

use std::fs::File;
use std::path::Path;

use memchr::memchr;

use crate::csv;

/// Bytes of `\n` after a chunk in its buffer, so SIMD loads never need a bounds check.
pub const PAD: usize = 128;

static POPULATE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var("SS_POPULATE").as_deref() != Ok("0"));
static DROP_PAGES: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var("SS_DROP").as_deref() != Ok("0"));

/// Unmaps the pages wholly inside `data` (a parsed range of a file mapping): the
/// teardown work is done now, by the parsing threads in parallel, not at exit. A
/// later read of these bytes just faults them back in from the page cache.
fn release(data: &[u8]) {
    #[cfg(target_os = "linux")]
    {
        let start = (data.as_ptr() as usize).div_ceil(4096) * 4096;
        let end = (data.as_ptr() as usize + data.len()) / 4096 * 4096;
        if end > start {
            // SAFETY: advice on pages of a read-only file mapping we own; their
            // contents stay readable (re-faulted from the page cache).
            unsafe {
                libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_DONTNEED);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = data;
}

/// Maps a chunk's pages in one call (Linux 5.14+) instead of one fault per page group.
fn populate(data: &[u8]) {
    #[cfg(target_os = "linux")]
    {
        const MADV_POPULATE_READ: libc::c_int = 22;
        let start = data.as_ptr() as usize & !4095;
        let end = data.as_ptr() as usize + data.len();
        // SAFETY: advice only; a failure (old kernel) is ignored.
        unsafe {
            libc::madvise(start as *mut libc::c_void, end - start, MADV_POPULATE_READ);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = data;
}

pub struct Input {
    pub files: Vec<File>,
    pub lens: Vec<u64>,
    /// Memory maps of the files, unless SS_IO=pread (then chunks are read with pread).
    maps: Vec<memmap2::Mmap>,
}

pub struct Chunk {
    pub file: usize,
    pub start: u64,
    pub end: u64,
}

fn list_csv(dir: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".csv") && !name.starts_with('.') && e.path().is_file()
        })
        .map(|e| e.path())
        .collect();
    paths.sort();
    paths
}

impl Input {
    pub fn open(dir: &Path) -> Self {
        let mut files = Vec::new();
        let mut lens = Vec::new();
        for path in list_csv(dir) {
            let file = File::open(&path).unwrap_or_else(|e| panic!("opening {}: {e}", path.display()));
            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if len > 0 {
                files.push(file);
                lens.push(len);
            }
        }
        let maps = if std::env::var("SS_IO").as_deref() != Ok("pread") {
            files
                .iter()
                // SAFETY: the inputs are read-only and nothing modifies them while we run.
                .map(|f| unsafe { memmap2::Mmap::map(f) }.expect("mmap failed"))
                .collect()
        } else {
            Vec::new()
        };
        Self { files, lens, maps }
    }

    /// Calls `f` with the fields of every well-formed line of the chunk: the data
    /// lines whose first byte lies in the chunk's range (the file's first line is its
    /// header; the last line may run past the range's end).
    #[inline]
    pub fn parse<const N: usize, F: FnMut(&[&[u8]; N])>(&self, chunk: &Chunk, buf: &mut Vec<u8>, simd: bool, f: &mut F) {
        if let Some(map) = self.maps.get(chunk.file) {
            let (start, end) = (chunk.start as usize, chunk.end as usize);
            // The first line start at or after `start` (start == 0: after the header).
            let from = start.saturating_sub(1);
            let Some(nl) = memchr(b'\n', &map[from..]) else { return };
            let first = from + nl + 1;
            if first < end {
                if *POPULATE {
                    populate(&map[first..end]);
                }
                range_lines::<N, F>(map, first, end, map.len(), simd, f);
            }
            if *DROP_PAGES {
                release(&map[start..end]);
            }
        } else {
            let n = self.read(chunk, buf);
            range_lines::<N, F>(buf, 0, n, n, simd, f);
        }
    }

    pub fn bytes(&self) -> u64 {
        self.lens.iter().sum()
    }

    /// Probe only: seconds to map the first 64 MiB of the largest file (slow when
    /// the page cache does not hold it yet).
    pub fn probe_cold(&self) -> f64 {
        let Some((file, _)) = self.lens.iter().enumerate().max_by_key(|&(_, &l)| l) else { return 0.0 };
        let Some(map) = self.maps.get(file) else { return 0.0 };
        let n = map.len().min(64 << 20);
        let started = std::time::Instant::now();
        populate(&map[..n]);
        started.elapsed().as_secs_f64()
    }

    /// The position just after the first `\n` at or after `from`, or the file length.
    fn next_line_start(&self, file: usize, from: u64) -> u64 {
        let len = self.lens[file];
        let mut buf = vec![0u8; 1 << 16];
        let mut at = from;
        while at < len {
            let n = ((len - at) as usize).min(buf.len());
            read_exact_at(&self.files[file], &mut buf[..n], at);
            if let Some(i) = memchr(b'\n', &buf[..n]) {
                return at + i as u64 + 1;
            }
            at += n as u64;
        }
        len
    }

    /// Chunks of about `size` bytes covering every data line. Mapped files: plain
    /// byte ranges, nothing read here (see `parse`). SS_IO=pread: line-aligned
    /// ranges, planned with reads (the first line of each file is its header).
    pub fn chunks(&self, size: usize) -> Vec<Chunk> {
        let size = size.max(1) as u64;
        let mut out = Vec::new();
        if !self.maps.is_empty() {
            for file in 0..self.files.len() {
                let len = self.lens[file];
                let mut start = 0;
                while start < len {
                    let end = (start + size).min(len);
                    out.push(Chunk { file, start, end });
                    start = end;
                }
            }
            return out;
        }
        for file in 0..self.files.len() {
            let len = self.lens[file];
            let mut start = self.next_line_start(file, 0);
            while start < len {
                let target = start + size;
                let end = if target >= len { len } else { self.next_line_start(file, target - 1) };
                out.push(Chunk { file, start, end });
                start = end;
            }
        }
        out
    }

    /// Reads a chunk into `buf` (followed by PAD bytes of `\n`); returns its length.
    pub fn read(&self, chunk: &Chunk, buf: &mut Vec<u8>) -> usize {
        let n = (chunk.end - chunk.start) as usize;
        if buf.len() < n + PAD {
            buf.resize(n + PAD, 0);
        }
        read_exact_at(&self.files[chunk.file], &mut buf[..n], chunk.start);
        buf[n..n + PAD].fill(b'\n');
        n
    }
}

fn read_exact_at(file: &File, mut buf: &mut [u8], mut offset: u64) {
    while !buf.is_empty() {
        #[cfg(unix)]
        let r = std::os::unix::fs::FileExt::read_at(file, buf, offset);
        #[cfg(windows)]
        let r = std::os::windows::fs::FileExt::seek_read(file, buf, offset);
        match r {
            Ok(0) => panic!("unexpected end of file"),
            Ok(n) => {
                buf = &mut buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => panic!("read failed: {e}"),
        }
    }
}

/// True if the AVX2 splitter can be used (and SS_SCALAR is not set).
pub fn use_simd() -> bool {
    if std::env::var_os("SS_SCALAR").is_some() {
        return false;
    }
    #[cfg(target_arch = "x86_64")]
    {
        is_x86_feature_detected!("avx2") && is_x86_feature_detected!("bmi1") && is_x86_feature_detected!("bmi2") && is_x86_feature_detected!("popcnt")
    }
    #[cfg(not(target_arch = "x86_64"))]
    false
}

/// Calls `f` with the fields of every well-formed line starting in `[start, end)`,
/// `start` being a line start and `data_len` the end of the data in `buf`. The
/// AVX2 path reads 128-byte windows wherever they fit inside `buf` (a chunk buffer
/// has PAD bytes of `\n` after its data, so they always fit there).
#[inline]
pub fn range_lines<const N: usize, F: FnMut(&[&[u8]; N])>(
    buf: &[u8],
    start: usize,
    end: usize,
    data_len: usize,
    simd: bool,
    f: &mut F,
) {
    #[cfg(target_arch = "x86_64")]
    if simd {
        // SAFETY: use_simd() checked the CPU features; blocks are bounds-checked.
        unsafe {
            if *OLD_TOKENIZER {
                simd_range::<N, F>(buf, start, end, data_len, f)
            } else {
                TOKENS.with(|t| staged_range::<N, F>(&mut t.borrow_mut(), buf, start, end, data_len, f))
            }
        };
        return;
    }
    let _ = simd;
    let mut out: [&[u8]; N] = [&[]; N];
    let mut p = start;
    while p < end {
        p = scalar_line::<N, F>(buf, p, data_len, &mut out, f);
    }
}

/// One line starting at p: calls `f` if it is well formed; returns the next line start.
#[inline]
fn scalar_line<'a, const N: usize, F: FnMut(&[&[u8]; N])>(
    buf: &'a [u8],
    p: usize,
    data_len: usize,
    out: &mut [&'a [u8]; N],
    f: &mut F,
) -> usize {
    let q = memchr(b'\n', &buf[p..data_len]).map_or(data_len, |i| p + i);
    if csv::fields::<N>(&buf[p..q], out) {
        f(out);
    }
    q + 1
}

#[cfg(target_arch = "x86_64")]
#[inline]
#[target_feature(enable = "avx2")]
unsafe fn block_masks(p: *const u8) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    let lo = _mm256_loadu_si256(p as *const __m256i);
    let hi = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let mask = |v: __m256i, c: u8| -> u32 { _mm256_movemask_epi8(_mm256_cmpeq_epi8(v, _mm256_set1_epi8(c as i8))) as u32 };
    let n = mask(lo, b'\n') as u64 | (mask(hi, b'\n') as u64) << 32;
    let c = mask(lo, b',') as u64 | (mask(hi, b',') as u64) << 32;
    let q = mask(lo, b'"') as u64 | (mask(hi, b'"') as u64) << 32;
    (n, c, q)
}

/// 1 when `buf[start..end]` is a quoted field (at least 2 bytes, a quote at each
/// end), else 0; without branches, as fields are quoted at random.
#[inline(always)]
unsafe fn quoted_field(buf: &[u8], start: usize, end: usize) -> usize {
    // Both reads stay inside the buffer for any width (an empty last field can
    // end the buffer); their values only matter when the width is at least 2.
    let top = buf.len().saturating_sub(1);
    let first = *buf.get_unchecked(start.min(top));
    let last = *buf.get_unchecked(end.saturating_sub(1).min(top));
    ((end >= start + 2) & (first == b'"') & (last == b'"')) as usize
}

/// Bits from `from` (0..=64) upwards.
#[inline(always)]
fn bits_from(from: usize) -> u64 {
    if from >= 64 { 0 } else { !0u64 << from }
}

/// The lines of `[start, end)` from 64-byte blocks: each byte is classified once,
/// then the commas and newlines are walked in order, the line state carrying
/// over from one block to the next. Same rules as csv::fields: exactly N-1 commas,
/// a field is quoted when it is at least 2 bytes long and starts and ends with a
/// quote, and the line is well formed exactly when it holds two quotes per quoted
/// field and no other quote.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi1,popcnt")]
unsafe fn simd_range<const N: usize, F: FnMut(&[&[u8]; N])>(
    buf: &[u8],
    start: usize,
    end: usize,
    data_len: usize,
    f: &mut F,
) {
    let mut out: [&[u8]; N] = [&[]; N];
    let mut line_start = start;
    let mut field_start = start;
    let mut k = 0usize; // fields completed in the current line
    let mut too_many = false;
    let mut quotes = 0u32; // quotes seen in the current line
    let mut quoted = 0u32; // quoted fields in the current line
    let mut block = start;
    while line_start < end {
        // Masks of this block; past the data (or the buffer) every byte is '\n'.
        let (nl, comma, quote) = if block + 64 <= data_len.min(buf.len()) {
            block_masks(buf.as_ptr().add(block))
        } else {
            let mut tail = [b'\n'; 64];
            let have = data_len.saturating_sub(block).min(64);
            tail[..have].copy_from_slice(&buf[block..block + have]);
            block_masks(tail.as_ptr())
        };
        // Only bytes at or after the current line start count.
        let skip = bits_from(line_start.saturating_sub(block));
        let mut structural = (nl | comma) & skip;
        let mut counted = line_start.saturating_sub(block); // quotes counted up to here
        while structural != 0 {
            let i = structural.trailing_zeros() as usize;
            structural &= structural - 1;
            let pos = block + i;
            if (nl >> i) & 1 == 0 {
                // a comma ends field k
                if k + 1 < N {
                    let q = quoted_field(buf, field_start, pos);
                    quoted += q as u32;
                    *out.get_unchecked_mut(k) = buf.get_unchecked(field_start + q..pos - q);
                    k += 1;
                } else {
                    too_many = true;
                }
                field_start = pos + 1;
                continue;
            }
            // a newline (or the end of the data) ends the line
            quotes += (quote & bits_from(counted) & !bits_from(i)).count_ones();
            counted = i + 1;
            let line_end = pos.min(data_len);
            if !too_many && k + 1 == N && line_end > line_start {
                let q = quoted_field(buf, field_start, line_end);
                quoted += q as u32;
                *out.get_unchecked_mut(N - 1) = buf.get_unchecked(field_start + q..line_end - q);
                if quotes == 2 * quoted {
                    f(&out);
                }
            }
            line_start = pos + 1;
            field_start = pos + 1;
            k = 0;
            too_many = false;
            quotes = 0;
            quoted = 0;
            if line_start >= end {
                return;
            }
        }
        quotes += (quote & bits_from(counted)).count_ones();
        block += 64;
    }
}

static OLD_TOKENIZER: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var("SS_TOK").as_deref() == Ok("old"));

/// Bytes tokenized by stage 1 before stage 2 runs, so the index arrays stay in cache.
const SUB: usize = 16 << 10;

/// Stage 1 output, reused by each thread.
struct Tokens {
    /// Separator (comma or newline) positions relative to the round's first block;
    /// slot 0 is the line start - 1.
    pos: Vec<u32>,
    /// Per newline: its index in `pos`, and the quotes before it in the round.
    nl_idx: Vec<u32>,
    nl_quotes: Vec<u32>,
    /// Bit i: the field after pos[i] starts with a quote.
    q_start: Vec<u64>,
    /// Bit i: the byte before pos[i] is a quote.
    q_end: Vec<u64>,
}

thread_local! {
    static TOKENS: std::cell::RefCell<Tokens> = std::cell::RefCell::new(Tokens {
        pos: vec![0; SUB + 1024],
        nl_idx: vec![0; SUB + 1024],
        nl_quotes: vec![0; SUB + 1024],
        q_start: vec![0; SUB / 64 + 64],
        q_end: vec![0; SUB / 64 + 64],
    });
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
        self.acc |= bits.checked_shl(self.fill).unwrap_or(0);
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

/// Bits i..i+N of a bit stream (N <= 8).
#[inline(always)]
unsafe fn read_bits<const N: usize>(stream: *const u64, i: usize) -> u32 {
    (((stream as *const u8).add(i >> 3) as *const u64).read_unaligned() >> (i & 7)) as u32 & ((1u32 << N) - 1)
}

/// The same lines as `simd_range`, in two stages per round of SUB bytes: stage 1
/// classifies 64-byte blocks into a flat array of separator positions, two bit
/// streams (field starts with a quote, byte before a separator is a quote) and, per
/// newline, its separator index and quote count; stage 2 takes each line whole: N - 1
/// commas, field bounds for all fields at once (8 lanes), quotes stripped without
/// branches. A field is quoted when it is at least 2 bytes long and starts and ends
/// with a quote; the line is well formed when its fields' start and end flags agree,
/// no 1-byte field is a lone quote, and it holds exactly two quotes per quoted field.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi1,bmi2,popcnt")]
unsafe fn staged_range<const N: usize, F: FnMut(&[&[u8]; N])>(
    t: &mut Tokens,
    buf: &[u8],
    start: usize,
    end: usize,
    data_len: usize,
    f: &mut F,
) {
    use std::arch::x86_64::*;
    const { assert!(N >= 2 && N <= 8) };
    let limit = data_len.min(buf.len());
    let b = buf.as_ptr();
    let pp = t.pos.as_mut_ptr();
    let lane_bit = _mm256_setr_epi32(1, 2, 4, 8, 16, 32, 64, 128);
    let active = _mm256_cmpgt_epi32(_mm256_set1_epi32(N as i32), _mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7));
    let mut out: [&[u8]; N] = [&[]; N];
    let mut line_start = start;
    while line_start < end {
        // ---- stage 1: the blocks from the one holding line_start; the last one
        // holds position `limit` (bytes from there on read as '\n')
        let at = line_start & !63;
        let stop = (at + SUB).min((limit & !63) + 64);
        *pp = ((line_start - at) as u32).wrapping_sub(1);
        let (mut np, mut nn, mut quotes) = (1usize, 0usize, 0u32);
        let mut starts_w = BitWriter { p: t.q_start.as_mut_ptr(), acc: 0, fill: 0 };
        let mut ends_w = BitWriter { p: t.q_end.as_mut_ptr(), acc: 0, fill: 0 };
        ends_w.put(0, 1); // keeps the q_end index equal to the pos index
        let mut live = !0u64 << (line_start & 63);
        let mut first_field = 1u64 << (line_start & 63);
        let (mut carry_sep, mut carry_quote) = (0u64, 0u64);
        let mut block = at;
        while block < stop {
            let (nl, comma, quote) = if block + 64 <= limit {
                block_masks(b.add(block))
            } else {
                let mut tail = [b'\n'; 64];
                let have = limit.saturating_sub(block).min(64);
                tail[..have].copy_from_slice(&buf[block..block + have]);
                block_masks(tail.as_ptr())
            };
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
                *t.nl_idx.get_unchecked_mut(nn + k) = np as u32 + (sep & below).count_ones();
                *t.nl_quotes.get_unchecked_mut(nn + k) = quotes + (quote & below).count_ones();
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
            // a line longer than the round: the scalar path takes it
            line_start = scalar_line::<N, F>(buf, line_start, data_len, &mut out, f);
            continue;
        }
        // ---- stage 2: every line that ends in these blocks
        let base = b.add(at);
        let (mut next_j, mut quotes_before) = (1usize, 0u32);
        for li in 0..nn {
            let j_end = *t.nl_idx.get_unchecked(li) as usize;
            let line_quotes = *t.nl_quotes.get_unchecked(li) - quotes_before;
            quotes_before = *t.nl_quotes.get_unchecked(li);
            let j = next_j;
            next_j = j_end + 1;
            let first = at + (*pp.add(j - 1)).wrapping_add(1) as usize;
            if first >= end {
                return;
            }
            line_start = at + *pp.add(j_end) as usize + 1;
            if j_end - j != N - 1 {
                continue; // not N fields (or an empty line)
            }
            let quoted = read_bits::<N>(t.q_start.as_ptr(), j - 1);
            if (quoted != read_bits::<N>(t.q_end.as_ptr(), j)) | (2 * quoted.count_ones() != line_quotes) {
                continue;
            }
            let lanes = _mm256_cmpeq_epi32(_mm256_and_si256(_mm256_set1_epi32(quoted as i32), lane_bit), lane_bit);
            let left = _mm256_loadu_si256(pp.add(j - 1) as *const __m256i);
            let right = _mm256_loadu_si256(pp.add(j) as *const __m256i);
            // a lone quote (a 1-byte field flagged at both ends) is not a quoted field
            let width = _mm256_sub_epi32(right, left); // field length + 1
            let lone = _mm256_and_si256(_mm256_and_si256(lanes, active), _mm256_cmpgt_epi32(_mm256_set1_epi32(3), width));
            if _mm256_movemask_epi8(lone) != 0 {
                continue;
            }
            let starts = _mm256_sub_epi32(_mm256_sub_epi32(left, _mm256_set1_epi32(-1)), lanes);
            let ends = _mm256_add_epi32(right, lanes);
            let (mut s, mut e) = ([0u32; 8], [0u32; 8]);
            _mm256_storeu_si256(s.as_mut_ptr() as *mut __m256i, starts);
            _mm256_storeu_si256(e.as_mut_ptr() as *mut __m256i, ends);
            for k in 0..N {
                *out.get_unchecked_mut(k) = std::slice::from_raw_parts(base.add(s[k] as usize), (e[k] - s[k]) as usize);
            }
            f(&out);
        }
    }
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

    fn collect<const N: usize>(text: &[u8], simd: bool) -> Vec<Vec<Vec<u8>>> {
        let mut buf = text.to_vec();
        buf.extend_from_slice(&[b'\n'; PAD]);
        let mut got = Vec::new();
        let mut f = |fields: &[&[u8]; N]| got.push(fields.iter().map(|x| x.to_vec()).collect());
        range_lines::<N, _>(&buf, 0, text.len(), text.len(), simd, &mut f);
        got
    }

    #[test]
    fn simd_matches_scalar() {
        if !use_simd() {
            return;
        }
        let alphabet = b"ab,,\"\" \r\nx1";
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        for round in 0..20_000 {
            let n = (rng(&mut s) % 400) as usize;
            let mut text: Vec<u8> = (0..n).map(|_| alphabet[(rng(&mut s) % alphabet.len() as u64) as usize]).collect();
            if round % 7 == 0 {
                // long lines too
                text.extend(std::iter::repeat_n(b'z', 150));
                text.extend_from_slice(b",q\n\"a\",\"\"\n");
            }
            assert_eq!(collect::<2>(&text, true), collect::<2>(&text, false), "{:?}", String::from_utf8_lossy(&text));
            // Unpadded, as a memory map is: windows near the end fall back to scalar.
            let mut a = Vec::new();
            let mut b = Vec::new();
            range_lines::<4, _>(&text, 0, text.len(), text.len(), true, &mut |x: &[&[u8]; 4]| a.push(x.map(|y| y.to_vec())));
            range_lines::<4, _>(&text, 0, text.len(), text.len(), false, &mut |x: &[&[u8]; 4]| b.push(x.map(|y| y.to_vec())));
            assert_eq!(a, b);
            assert_eq!(collect::<4>(&text, true), collect::<4>(&text, false));
            assert_eq!(collect::<7>(&text, true), collect::<7>(&text, false));
        }
    }

    /// Lines shaped like the real ones (quoted fields, lone quotes, wrong field
    /// counts, a quote inside a field) and lines longer than a stage-1 round, over
    /// ranges that start and end anywhere: the staged tokenizer, the old SIMD one and
    /// the scalar one agree.
    #[test]
    fn staged_matches_scalar_on_csv_like_text() {
        if !use_simd() {
            return;
        }
        let pieces: &[&[u8]] = &[b"e1", b"\"u2\"", b"", b"\"\"", b"\"", b"a\"b", b"\"x", b"y\"", b"2027-01-01T00:00:00Z", b"view", b"\r", b" ", b"\"\"\""];
        let mut s = 0x2545_f491_4f6c_dd1du64;
        for round in 0..3000 {
            let mut text = Vec::new();
            let lines = 1 + rng(&mut s) % 60;
            for _ in 0..lines {
                let fields = if rng(&mut s) % 8 == 0 { 1 + rng(&mut s) % 9 } else { 7 };
                for k in 0..fields {
                    if k > 0 {
                        text.push(b',');
                    }
                    if round % 50 == 0 && rng(&mut s) % 40 == 0 {
                        text.extend(std::iter::repeat_n(b'q', 17_000 + (rng(&mut s) % 3000) as usize)); // > SUB
                    } else {
                        text.extend_from_slice(pieces[(rng(&mut s) % pieces.len() as u64) as usize]);
                    }
                }
                if rng(&mut s) % 30 != 0 {
                    text.push(b'\n');
                }
            }
            let n = text.len();
            // a range [start, end) whose start is a line start
            let cut = (rng(&mut s) % (n as u64 + 1)) as usize;
            let start = match text[..cut].iter().rposition(|&c| c == b'\n') {
                Some(i) => i + 1,
                None => 0,
            };
            let end = start + (rng(&mut s) % (n - start + 1) as u64) as usize;
            for padded in [false, true] {
                let mut buf = text.clone();
                if padded {
                    buf.extend_from_slice(&[b'\n'; PAD]);
                }
                let run = |old: bool, simd: bool| {
                    let mut got: Vec<Vec<Vec<u8>>> = Vec::new();
                    let mut f = |x: &[&[u8]; 7]| got.push(x.iter().map(|y| y.to_vec()).collect());
                    if old {
                        unsafe { simd_range::<7, _>(&buf, start, end, n, &mut f) };
                    } else {
                        range_lines::<7, _>(&buf, start, end, n, simd, &mut f);
                    }
                    got
                };
                let scalar = run(false, false);
                assert_eq!(run(false, true), scalar, "round {round} padded {padded}");
                assert_eq!(run(true, true), scalar, "round {round} padded {padded} (old)");
            }
        }
    }
}
