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
            Self { ptr: aligned as *mut T }
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn new(n: usize) -> Self {
        let mut keep = Vec::with_capacity(n.max(1));
        let ptr = keep.as_mut_ptr() as *mut T;
        Self { ptr, _keep: keep }
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

/// Values scattered into partitions, stored in blocks carved in order from one
/// per-thread arena: the memory touched (and zeroed by the kernel) is the data
/// actually written, not a reserved capacity per partition.
pub struct Scatter<T: Copy> {
    region: Region<T>,
    capacity: usize,
    used: usize,
    block: usize,
    /// Per partition: its blocks (as block numbers), and how full the last one is.
    blocks: Vec<Vec<u32>>,
    current: Vec<usize>,
    fill: Vec<usize>,
    overflow: Vec<Vec<T>>,
    _marker: PhantomData<T>,
}

// SAFETY: a Scatter owns its memory exclusively, like a Vec<T>.
unsafe impl<T: Copy + Send> Send for Scatter<T> {}
unsafe impl<T: Copy + Sync> Sync for Scatter<T> {}

impl<T: Copy> Scatter<T> {
    /// `parts` partitions, about `expected` values in all (an estimate), room for
    /// `capacity` values in the arena before spilling into Vecs.
    pub fn new(parts: usize, expected: usize, capacity: usize) -> Self {
        let block = (expected / parts.max(1) / 8).clamp(16, 1024);
        let capacity = capacity + parts * block;
        Self {
            region: Region::new(capacity),
            capacity,
            used: 0,
            block,
            blocks: (0..parts).map(|_| Vec::new()).collect(),
            current: vec![0; parts],
            fill: vec![block; parts],
            overflow: (0..parts).map(|_| Vec::new()).collect(),
            _marker: PhantomData,
        }
    }

    pub fn parts(&self) -> usize {
        self.blocks.len()
    }

    #[inline]
    pub fn push(&mut self, p: usize, value: T) {
        let f = self.fill[p];
        if f < self.block {
            // SAFETY: current[p] + f lies inside a block of the region.
            unsafe { self.region.ptr.add(self.current[p] + f).write(value) };
            self.fill[p] = f + 1;
        } else {
            self.push_new_block(p, value);
        }
    }

    #[cold]
    fn push_new_block(&mut self, p: usize, value: T) {
        if self.used + self.block <= self.capacity {
            let start = self.used;
            self.used += self.block;
            self.blocks[p].push((start / self.block) as u32);
            self.current[p] = start;
            // SAFETY: start is the beginning of a fresh block inside the region.
            unsafe { self.region.ptr.add(start).write(value) };
            self.fill[p] = 1;
        } else {
            self.overflow[p].push(value);
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
                unsafe { std::slice::from_raw_parts(self.region.ptr.add(b as usize * self.block), len) }
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

#[cfg(test)]
mod tests {
    use super::Scatter;

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
