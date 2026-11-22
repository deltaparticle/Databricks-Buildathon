//! Big scatter buffers without page-fault storms: one contiguous allocation per
//! thread, backed by transparent huge pages on Linux, cut into a fixed-capacity
//! region per partition. A partition that outgrows its region spills into a Vec.

use std::marker::PhantomData;

/// Raw memory for `n` values of T: anonymous huge-page mapping on Linux, a plain
/// allocation elsewhere. Never freed before exit on Linux (the process ends first).
struct Region<T> {
    ptr: *mut T,
    #[cfg(not(target_os = "linux"))]
    _keep: Vec<std::mem::MaybeUninit<T>>,
}

#[cfg(target_os = "linux")]
const HUGE: usize = 2 << 20;

static PREFAULT_SET: std::sync::Once = std::sync::Once::new();
static PREFAULT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether arenas are prefaulted 2 MiB at a time (SS_PREFAULT=1, or `set_prefault`).
#[inline]
fn prefault_on() -> bool {
    PREFAULT_SET.call_once(|| PREFAULT.store(std::env::var("SS_PREFAULT").as_deref() == Ok("1"), std::sync::atomic::Ordering::Relaxed));
    PREFAULT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Probe only: turns prefaulting on or off for the arenas carved from now on.
pub fn set_prefault(on: bool) {
    let _ = prefault_on();
    PREFAULT.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// With SS_PREFAULT=1: when `offset..offset + len` (bytes from a 2 MiB-aligned region
/// start) enters a new 2 MiB step, maps that whole step in one call
/// (MADV_POPULATE_WRITE) instead of one page fault per 4 KiB page.
#[inline]
fn prefault_step(base: *mut u8, offset: usize, len: usize, region_bytes: usize) {
    #[cfg(target_os = "linux")]
    if (offset % HUGE == 0 || (offset + len - 1) / HUGE != offset / HUGE) && prefault_on() {
        let step = (offset + len - 1) / HUGE * HUGE;
        let n = HUGE.min(region_bytes.saturating_sub(step));
        if n > 0 {
            const MADV_POPULATE_WRITE: libc::c_int = 23;
            // SAFETY: advice on memory of our own mapping; failures are ignored.
            unsafe {
                libc::madvise(base.add(step) as *mut libc::c_void, n, MADV_POPULATE_WRITE);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (base, offset, len, region_bytes);
}

/// Every region mapped so far (start, bytes), for `release_all`.
#[cfg(target_os = "linux")]
static MAPPED: std::sync::Mutex<Vec<(usize, usize)>> = std::sync::Mutex::new(Vec::new());

/// Unmaps every region on `threads` threads, so that the kernel has almost nothing
/// left to tear down (serially) when the process exits. Nothing may touch arena
/// memory afterwards: call it right before `_exit`.
pub fn release_all(threads: usize) {
    #[cfg(target_os = "linux")]
    {
        const PIECE: usize = 64 << 20;
        let regions = std::mem::take(&mut *MAPPED.lock().unwrap());
        let mut pieces = Vec::new();
        for (start, bytes) in regions {
            let mut at = 0;
            while at < bytes {
                let n = PIECE.min(bytes - at);
                pieces.push((start + at, n));
                at += n;
            }
        }
        let next = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads.clamp(1, pieces.len().max(1)) {
                s.spawn(|| loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(&(start, n)) = pieces.get(i) else { break };
                    // SAFETY: a piece of a mapping of ours that nothing uses any more
                    // (the caller exits right after).
                    unsafe {
                        libc::munmap(start as *mut libc::c_void, n);
                    }
                });
            }
        });
    }
    #[cfg(not(target_os = "linux"))]
    let _ = threads;
}

impl<T> Region<T> {
    #[cfg(target_os = "linux")]
    fn new(n: usize) -> Self {
        let bytes = (n.max(1) * std::mem::size_of::<T>()).div_ceil(HUGE) * HUGE;
        // SAFETY: a fresh private anonymous mapping; we only hand out pointers into it.
        unsafe {
            let raw = libc::mmap(
                std::ptr::null_mut(),
                bytes + HUGE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            );
            assert!(raw != libc::MAP_FAILED, "mmap of {bytes} bytes failed");
            let aligned = ((raw as usize + HUGE - 1) & !(HUGE - 1)) as *mut libc::c_void;
            let advice = if std::env::var("SS_THP").as_deref() == Ok("0") {
                libc::MADV_NOHUGEPAGE
            } else {
                libc::MADV_HUGEPAGE
            };
            libc::madvise(aligned, bytes, advice);
            MAPPED.lock().unwrap().push((raw as usize, bytes + HUGE));
            Self { ptr: aligned as *mut T }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn new(n: usize) -> Self {
        let bytes = (n.max(1) * std::mem::size_of::<T>()).max(64);
        let layout = std::alloc::Layout::from_size_align(bytes, 4096).unwrap();
        // SAFETY: a non-zero-sized layout; the memory is never freed (the process ends first).
        let ptr = unsafe { std::alloc::alloc(layout) } as *mut T;
        assert!(!ptr.is_null(), "allocation of {bytes} bytes failed");
        Self { ptr, _keep: Vec::new() }
    }
}

/// A zero-filled slice of n values that lives until exit, on huge pages on Linux.
/// T must be valid when all-zero.
pub fn zeroed_slice<T: Copy>(n: usize) -> &'static mut [T] {
    let region = Region::<T>::new(n);
    let ptr = region.ptr;
    #[cfg(not(target_os = "linux"))]
    {
        // SAFETY: the allocation holds n values; zero them, then keep it alive forever.
        unsafe { std::ptr::write_bytes(ptr, 0, n.max(1)) };
        std::mem::forget(region);
    }
    // SAFETY: n values of zeroed (anonymous mappings are zero-filled) memory, never freed.
    unsafe { std::slice::from_raw_parts_mut(ptr, n) }
}

/// A Vec of `n` values whose buffer is advised to use transparent huge pages,
/// so filling it costs a few faults instead of one per 4 KiB.
pub fn huge_vec<T>(n: usize) -> Vec<T> {
    let v: Vec<T> = Vec::with_capacity(n);
    #[cfg(target_os = "linux")]
    {
        const HUGE_PAGE: usize = 2 << 20;
        let start = (v.as_ptr() as usize).div_ceil(HUGE_PAGE) * HUGE_PAGE;
        let end = (v.as_ptr() as usize + n * std::mem::size_of::<T>()) / HUGE_PAGE * HUGE_PAGE;
        if end > start {
            // SAFETY: advice on memory this Vec owns.
            unsafe {
                libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_HUGEPAGE);
            }
        }
    }
    v
}

/// Values scattered into partitions, stored in blocks carved in order from one
/// per-thread arena: the memory touched (and zeroed by the kernel) is the data
/// actually written, not a reserved capacity per partition.
pub struct Scatter<T: Copy> {
    region: Region<T>,
    capacity: usize,
    used: usize,
    block: usize,
    /// Per partition: its blocks, and how full the last one is.
    blocks: Vec<Vec<*mut T>>,
    current: Vec<*mut T>,
    fill: Vec<usize>,
    /// Blocks handed over by `adopt` (memory already touched), used before the arena.
    free: Vec<*mut T>,
    overflow: Vec<Vec<T>>,
    /// Software write-combining: per partition, one cache line of values waiting
    /// to be written with non-temporal stores (when a line holds whole values).
    staging: Vec<Line>,
    staged: Vec<u8>,
    per_line: usize,
    _marker: PhantomData<T>,
}

#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Line([u8; 64]);

/// Block size of the write-combined scatters, the same for every record type so
/// that blocks can move from one scatter to another.
pub const BLOCK_BYTES: usize = 4096;

/// Copies one 64-byte line with non-temporal stores (no read of the destination).
#[inline(always)]
unsafe fn stream_line(dst: *mut u8, src: *const u8) {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        let (d, s) = (dst as *mut __m128i, src as *const __m128i);
        _mm_stream_si128(d, _mm_load_si128(s));
        _mm_stream_si128(d.add(1), _mm_load_si128(s.add(1)));
        _mm_stream_si128(d.add(2), _mm_load_si128(s.add(2)));
        _mm_stream_si128(d.add(3), _mm_load_si128(s.add(3)));
    }
    #[cfg(not(target_arch = "x86_64"))]
    std::ptr::copy_nonoverlapping(src, dst, 64);
}

// SAFETY: a Scatter owns its memory exclusively, like a Vec<T>.
unsafe impl<T: Copy + Send> Send for Scatter<T> {}
unsafe impl<T: Copy + Sync> Sync for Scatter<T> {}

impl<T: Copy> Scatter<T> {
    /// `parts` partitions, about `expected` values in all (an estimate), room for
    /// `capacity` values in the arena before spilling into Vecs.
    pub fn new(parts: usize, expected: usize, capacity: usize) -> Self {
        let size = std::mem::size_of::<T>();
        let per_line = if size > 0 && size <= 64 && 64 % size == 0 { 64 / size } else { 0 };
        let block = if per_line > 0 {
            BLOCK_BYTES / size // whole lines, and the same bytes for every such T
        } else {
            (expected / parts.max(1) / 8).clamp(16, 1024)
        };
        let capacity = capacity + parts * block;
        Self {
            region: Region::new(capacity),
            capacity,
            used: 0,
            block,
            blocks: (0..parts).map(|_| Vec::new()).collect(),
            current: vec![std::ptr::null_mut(); parts],
            fill: vec![block; parts],
            free: Vec::new(),
            overflow: (0..parts).map(|_| Vec::new()).collect(),
            staging: vec![Line([0; 64]); if per_line > 0 { parts } else { 0 }],
            staged: vec![0; parts],
            per_line,
            _marker: PhantomData,
        }
    }

    /// Like push, through the partition's staging line (T must divide 64 bytes).
    /// `finish` must run before the values are read.
    #[inline(always)]
    pub fn push_wc(&mut self, p: usize, value: T) {
        debug_assert!(self.per_line > 0);
        let n = self.staged[p] as usize;
        // SAFETY: n < per_line, so the value lies inside the 64-byte line.
        unsafe { (self.staging.as_mut_ptr().add(p) as *mut T).add(n).write(value) };
        if n + 1 < self.per_line {
            self.staged[p] = (n + 1) as u8;
        } else {
            self.staged[p] = 0;
            self.flush_line(p);
        }
    }

    /// A block for partition p (recycled first), or None when the arena is full.
    #[inline]
    fn new_block(&mut self, p: usize) -> Option<*mut T> {
        let b = match self.free.pop() {
            Some(b) => b,
            None if self.used + self.block <= self.capacity => {
                // SAFETY: used + block <= capacity, inside the region.
                let b = unsafe { self.region.ptr.add(self.used) };
                let size = std::mem::size_of::<T>();
                prefault_step(self.region.ptr as *mut u8, self.used * size, self.block * size, self.capacity * size);
                self.used += self.block;
                b
            }
            None => return None,
        };
        self.blocks[p].push(b);
        self.current[p] = b;
        self.fill[p] = 0;
        Some(b)
    }

    #[inline(never)]
    fn flush_line(&mut self, p: usize) {
        if self.fill[p] == self.block && self.new_block(p).is_none() {
            // SAFETY: the staging line holds per_line written values.
            let values = unsafe { std::slice::from_raw_parts(self.staging.as_ptr().add(p) as *const T, self.per_line) };
            self.overflow[p].extend_from_slice(values);
            return;
        }
        // SAFETY: blocks are whole, 64-byte aligned lines and fill counts whole lines.
        unsafe {
            let dst = self.current[p].add(self.fill[p]) as *mut u8;
            stream_line(dst, self.staging.as_ptr().add(p) as *const u8);
        }
        self.fill[p] += self.per_line;
    }

    /// The blocks of partition p as raw memory, to be handed to `adopt` once its
    /// values are no longer needed.
    pub fn blocks_of(&self, p: usize) -> &[*mut T] {
        &self.blocks[p]
    }

    /// Takes over blocks of BLOCK_BYTES bytes (from another Scatter whose values in
    /// them are dead) to fill before carving fresh arena memory.
    pub fn adopt<U>(&mut self, blocks: &[*mut U]) {
        debug_assert!(self.block * std::mem::size_of::<T>() == BLOCK_BYTES);
        self.free.extend(blocks.iter().map(|&b| b as *mut T));
    }

    /// Benchmark only: forgets every value and refills the same memory from the start.
    pub fn reset(&mut self) {
        self.used = 0;
        for b in &mut self.blocks {
            b.clear();
        }
        self.current.fill(std::ptr::null_mut());
        self.fill.fill(self.block);
        self.free.clear();
        for o in &mut self.overflow {
            o.clear();
        }
        self.staged.fill(0);
        let size = std::mem::size_of::<T>();
        self.per_line = if size > 0 && size <= 64 && 64 % size == 0 { 64 / size } else { 0 };
    }

    /// Writes out the partly filled staging lines; after this only `push` may be used.
    pub fn finish(&mut self) {
        for p in 0..self.staged.len() {
            let n = self.staged[p] as usize;
            for i in 0..n {
                // SAFETY: the first n values of the line were written by push_wc.
                let value = unsafe { (self.staging.as_ptr().add(p) as *const T).add(i).read() };
                self.push(p, value);
            }
            self.staged[p] = 0;
        }
        self.per_line = 0;
        #[cfg(target_arch = "x86_64")]
        // SAFETY: a store fence orders the non-temporal stores before later ones.
        unsafe {
            std::arch::x86_64::_mm_sfence()
        };
    }

    pub fn parts(&self) -> usize {
        self.blocks.len()
    }

    #[inline]
    pub fn push(&mut self, p: usize, value: T) {
        let f = self.fill[p];
        if f < self.block {
            // SAFETY: current[p] + f lies inside the partition's current block.
            unsafe { self.current[p].add(f).write(value) };
            self.fill[p] = f + 1;
        } else {
            self.push_new_block(p, value);
        }
    }

    #[cold]
    fn push_new_block(&mut self, p: usize, value: T) {
        match self.new_block(p) {
            Some(b) => {
                // SAFETY: b is the start of a block of `block` values.
                unsafe { b.write(value) };
                self.fill[p] = 1;
            }
            None => self.overflow[p].push(value),
        }
    }

    /// The values of partition p, as slices.
    #[inline]
    pub fn part(&self, p: usize) -> impl Iterator<Item = &[T]> + '_ {
        let blocks = &self.blocks[p];
        let last = blocks.len().wrapping_sub(1);
        blocks
            .iter()
            .enumerate()
            .map(move |(i, &b)| {
                let len = if i == last { self.fill[p] } else { self.block };
                // SAFETY: the first len values of the block were written by push.
                unsafe { std::slice::from_raw_parts(b as *const T, len) }
            })
            .chain(std::iter::once(self.overflow[p].as_slice()))
    }

    pub fn len(&self, p: usize) -> usize {
        let n = self.blocks[p].len();
        let in_blocks = if n == 0 { 0 } else { (n - 1) * self.block + self.fill[p] };
        in_blocks + self.overflow[p].len()
    }

    pub fn total(&self) -> usize {
        (0..self.parts()).map(|p| self.len(p)).sum()
    }
}

/// Words of a BLOCK_BYTES unit; a Words3 block uses the first WORDS3_BLOCK.
const UNIT_WORDS: usize = BLOCK_BYTES / 8;
/// 168 records of 3 words, 63 whole lines: no record and no line crosses a block.
const WORDS3_BLOCK: usize = 504;

/// Records of three words scattered into partitions, write-combined like
/// `Scatter::push_wc` but word by word, so a record may span two staging lines
/// (never two blocks). Blocks start BLOCK_BYTES apart, so a `Scatter` can adopt
/// them. When the arena is used up, another one is mapped.
pub struct Words3 {
    regions: Vec<Region<u64>>,
    /// Words per region.
    capacity: usize,
    /// Words carved from the last region.
    used: usize,
    blocks: Vec<Vec<*mut u64>>,
    current: Vec<*mut u64>,
    /// Words written to the current block.
    fill: Vec<u32>,
    staging: Vec<Line>,
    /// Words waiting in the staging line.
    staged: Vec<u8>,
}

// SAFETY: a Words3 owns its memory exclusively.
unsafe impl Send for Words3 {}
unsafe impl Sync for Words3 {}

impl Words3 {
    /// `parts` partitions, room for about `records` records before mapping more.
    pub fn new(parts: usize, records: usize) -> Self {
        let capacity = (records * 3).div_ceil(WORDS3_BLOCK) * UNIT_WORDS + parts * UNIT_WORDS;
        Self {
            regions: vec![Region::new(capacity)],
            capacity,
            used: 0,
            blocks: (0..parts).map(|_| Vec::new()).collect(),
            current: vec![std::ptr::null_mut(); parts],
            fill: vec![WORDS3_BLOCK as u32; parts],
            staging: vec![Line([0; 64]); parts],
            staged: vec![0; parts],
        }
    }

    pub fn parts(&self) -> usize {
        self.blocks.len()
    }

    #[inline(always)]
    pub fn push(&mut self, p: usize, record: [u64; 3]) {
        let mut n = self.staged[p] as usize;
        let line = self.staging[p].0.as_mut_ptr() as *mut u64;
        for word in record {
            // SAFETY: n < 8: inside the 64-byte staging line.
            unsafe { line.add(n).write(word) };
            n += 1;
            if n == 8 {
                self.flush_line(p);
                n = 0;
            }
        }
        self.staged[p] = n as u8;
    }

    #[inline(always)]
    fn flush_line(&mut self, p: usize) {
        if self.fill[p] as usize == WORDS3_BLOCK {
            self.new_block(p);
        }
        // SAFETY: fill + 8 <= WORDS3_BLOCK words, inside the block; 64-byte aligned.
        unsafe {
            let dst = self.current[p].add(self.fill[p] as usize) as *mut u8;
            stream_line(dst, self.staging[p].0.as_ptr());
        }
        self.fill[p] += 8;
    }

    #[inline(never)]
    fn new_block(&mut self, p: usize) {
        if self.used + UNIT_WORDS > self.capacity {
            self.regions.push(Region::new(self.capacity));
            self.used = 0;
        }
        // SAFETY: used + UNIT_WORDS <= capacity, inside the last region.
        let base = self.regions.last().unwrap().ptr;
        let b = unsafe { base.add(self.used) };
        prefault_step(base as *mut u8, self.used * 8, UNIT_WORDS * 8, self.capacity * 8);
        self.used += UNIT_WORDS;
        self.blocks[p].push(b);
        self.current[p] = b;
        self.fill[p] = 0;
    }

    /// Benchmark only: forgets every record and refills the first region from the start.
    pub fn reset(&mut self) {
        self.regions.truncate(1);
        self.used = 0;
        for b in &mut self.blocks {
            b.clear();
        }
        self.current.fill(std::ptr::null_mut());
        self.fill.fill(WORDS3_BLOCK as u32);
        self.staged.fill(0);
    }

    /// Writes out the staged words (which end whole records); call before reading.
    pub fn finish(&mut self) {
        for p in 0..self.staged.len() {
            let n = self.staged[p] as usize;
            if n == 0 {
                continue;
            }
            if self.fill[p] as usize == WORDS3_BLOCK {
                self.new_block(p);
            }
            // SAFETY: fill is a multiple of 8 below 504 and n < 8: inside the block.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.staging[p].0.as_ptr() as *const u64,
                    self.current[p].add(self.fill[p] as usize),
                    n,
                )
            };
            self.fill[p] += n as u32;
            self.staged[p] = 0;
        }
        #[cfg(target_arch = "x86_64")]
        // SAFETY: a store fence orders the non-temporal stores before later ones.
        unsafe {
            std::arch::x86_64::_mm_sfence()
        };
    }

    /// The records of partition p (after `finish`), as slices.
    #[inline]
    pub fn part(&self, p: usize) -> impl Iterator<Item = &[[u64; 3]]> + '_ {
        let blocks = &self.blocks[p];
        let last = blocks.len().wrapping_sub(1);
        blocks.iter().enumerate().map(move |(i, &b)| {
            let words = if i == last { self.fill[p] as usize } else { WORDS3_BLOCK };
            // SAFETY: the block's first `words` words were written, whole records.
            unsafe { std::slice::from_raw_parts(b as *const [u64; 3], words / 3) }
        })
    }

    /// The blocks of partition p, BLOCK_BYTES apart, for `Scatter::adopt`.
    pub fn blocks_of(&self, p: usize) -> &[*mut u64] {
        &self.blocks[p]
    }

    /// Records in partition p (after `finish`).
    pub fn len(&self, p: usize) -> usize {
        self.part(p).map(<[[u64; 3]]>::len).sum()
    }

    pub fn total(&self) -> usize {
        (0..self.parts()).map(|p| self.len(p)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::{Scatter, Words3};

    #[test]
    fn words3_keep_order_across_lines_blocks_and_regions() {
        for records in [0usize, 10, 1000] {
            let mut s = Words3::new(5, records);
            for i in 0..20_003u64 {
                s.push((i * 7 % 5) as usize, [i, !i, i * 3]);
            }
            s.finish();
            let mut total = 0;
            for p in 0..5 {
                let all: Vec<[u64; 3]> = s.part(p).flatten().copied().collect();
                let want: Vec<[u64; 3]> = (0..20_003).filter(|i| i * 7 % 5 == p as u64).map(|i| [i, !i, i * 3]).collect();
                assert_eq!(all, want);
                total += all.len();
            }
            assert_eq!(total, 20_003);
            assert_eq!(s.total(), 20_003);
            // its blocks can be adopted by a Scatter of 16-byte values
            let mut b: Scatter<[u64; 2]> = Scatter::new(2, 100, 0);
            for p in 0..5 {
                b.adopt(s.blocks_of(p));
            }
            for i in 0..30_000u64 {
                b.push_wc((i % 2) as usize, [i, i]);
            }
            b.finish();
            assert_eq!(b.total(), 30_000);
        }
    }

    #[test]
    fn write_combining_matches() {
        for capacity in [0, 7, 64, 100, 5000] {
            let mut s: Scatter<[u64; 4]> = Scatter::new(5, 300, capacity);
            for i in 0..3001u64 {
                s.push_wc((i * 7 % 5) as usize, [i, i + 1, i + 2, i + 3]);
            }
            s.finish();
            for p in 0..5 {
                let all: Vec<u64> = s.part(p).flatten().map(|v| v[0]).collect();
                assert_eq!(all, (0..3001).filter(|i| i * 7 % 5 == p as u64).collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn adopted_blocks_are_reused() {
        let mut a: Scatter<[u64; 4]> = Scatter::new(3, 1000, 2000);
        for i in 0..1500u64 {
            a.push_wc((i % 3) as usize, [i; 4]);
        }
        a.finish();
        let mut b: Scatter<[u64; 2]> = Scatter::new(2, 100, 0); // almost no arena of its own
        for p in 0..3 {
            let blocks: Vec<_> = a.blocks_of(p).to_vec();
            b.adopt(&blocks);
        }
        for i in 0..1200u64 {
            b.push_wc((i % 2) as usize, [i, i]);
        }
        b.finish();
        for p in 0..2 {
            let all: Vec<u64> = b.part(p).flatten().map(|v| v[0]).collect();
            assert_eq!(all, (0..1200).filter(|i| i % 2 == p as u64).collect::<Vec<_>>());
        }
    }

    #[test]
    fn blocks_spill_and_keep_order() {
        for capacity in [0, 5, 40, 1000] {
            let mut s: Scatter<u64> = Scatter::new(3, 48, capacity);
            for i in 0..3000u64 {
                s.push((i % 3) as usize, i);
            }
            for p in 0..3 {
                let all: Vec<u64> = s.part(p).flatten().copied().collect();
                assert_eq!(all, (0..3000).filter(|i| i % 3 == p as u64).collect::<Vec<_>>());
                assert_eq!(s.len(p), 1000);
            }
            assert_eq!(s.total(), 3000);
        }
    }
}
