//! Linked IDs: union-find over every link, each person named by the byte-wise
//! smallest of their IDs.

use std::cmp::Ordering;

use crate::hash::mix;
use crate::key;

/// Open addressing from a linked user key to its person's name: one 16-byte slot
/// (key, name) per entry, so a lookup touches a single cache line. Key 0 is empty.
pub struct Names {
    slots: &'static mut [[u64; 2]],
    mask: usize,
}

impl Names {
    fn with_entries(n: usize) -> Self {
        let size = (n.max(8) * 2).next_power_of_two();
        Self {
            slots: crate::arena::zeroed_slice(size),
            mask: size - 1,
        }
    }

    /// The person's name for a user key: itself if it appears in no link.
    #[inline]
    pub fn get(&self, user: u64) -> u64 {
        let mut i = mix(user) as usize & self.mask;
        loop {
            let [k, name] = self.slots[i];
            if k == user {
                return name;
            }
            if k == 0 {
                return user;
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Starts loading the slot `get(user)` will read first.
    #[inline]
    pub fn prefetch(&self, user: u64) {
        let i = mix(user) as usize & self.mask;
        prefetch(self.slots.as_ptr().wrapping_add(i));
    }
}

#[inline]
pub fn prefetch<T>(ptr: *const T) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: prefetching never faults, whatever the address.
    unsafe {
        std::arch::x86_64::_mm_prefetch(ptr as *const i8, std::arch::x86_64::_MM_HINT_T0);
    }
}

/// During the unions a slot's second word holds its node: parent (low 32 bits) and
/// size (high 32 bits), so following a parent touches the line that holds its key.
#[inline]
fn parent_of(slots: &[[u64; 2]], x: u32) -> u32 {
    slots[x as usize][1] as u32
}

#[inline]
fn find(slots: &mut [[u64; 2]], mut x: u32) -> u32 {
    loop {
        let p = parent_of(slots, x);
        if p == x {
            return x;
        }
        let grand = parent_of(slots, p);
        let word = &mut slots[x as usize][1];
        *word = (*word & !0xffff_ffff) | grand as u64;
        x = grand;
    }
}

/// Union-find over every linked ID, directly on the slots of the name table: the
/// slot of a key is its node. Each pair's slots are prefetched LAG pairs ahead.
pub fn build(pairs: &[Vec<(u64, u64)>]) -> Names {
    const LAG: usize = 16;
    let t0 = std::time::Instant::now();
    let total: usize = pairs.iter().map(Vec::len).sum();
    let names = Names::with_entries(2 * total + 16);
    let mask = names.mask;
    let mut occupied: Vec<u32> = Vec::with_capacity(2 * total + 16);
    let t1 = t0.elapsed().as_secs_f64();

    let slot_of = |slots: &mut [[u64; 2]], occupied: &mut Vec<u32>, key: u64| -> u32 {
        let mut i = mix(key) as usize & mask;
        loop {
            let k = slots[i][0];
            if k == key {
                return i as u32;
            }
            if k == 0 {
                slots[i] = [key, (1u64 << 32) | i as u64];
                occupied.push(i as u32);
                return i as u32;
            }
            i = (i + 1) & mask;
        }
    };
    let mut ahead = pairs.iter().flatten().skip(LAG);
    for &(a, b) in pairs.iter().flatten() {
        if let Some(&(x, y)) = ahead.next() {
            names.prefetch(x);
            names.prefetch(y);
        }
        let ia = slot_of(names.slots, &mut occupied, a);
        let ib = slot_of(names.slots, &mut occupied, b);
        let ra = find(names.slots, ia);
        let rb = find(names.slots, ib);
        if ra != rb {
            let size_a = names.slots[ra as usize][1] >> 32;
            let size_b = names.slots[rb as usize][1] >> 32;
            let (big, small) = if size_a >= size_b { (ra, rb) } else { (rb, ra) };
            names.slots[small as usize][1] = big as u64;
            names.slots[big as usize][1] = ((size_a + size_b) << 32) | big as u64;
        }
    }
    let t2 = t0.elapsed().as_secs_f64();
    // Roots first, then the smallest key of each person in its root's second word,
    // then that name in every member's second word.
    let root_of: Vec<u32> = occupied.iter().map(|&i| find(names.slots, i)).collect();
    for (&i, &r) in occupied.iter().zip(&root_of) {
        if i == r {
            names.slots[i as usize][1] = 0;
        }
    }
    for (n, (&i, &r)) in occupied.iter().zip(&root_of).enumerate() {
        if let Some(&r2) = root_of.get(n + LAG) {
            prefetch(names.slots.as_ptr().wrapping_add(r2 as usize));
        }
        let k = names.slots[i as usize][0];
        let best = names.slots[r as usize][1];
        if best == 0 || key::cmp(k, best) == Ordering::Less {
            names.slots[r as usize][1] = k;
        }
    }
    for (n, (&i, &r)) in occupied.iter().zip(&root_of).enumerate() {
        if let Some(&r2) = root_of.get(n + LAG) {
            prefetch(names.slots.as_ptr().wrapping_add(r2 as usize));
        }
        names.slots[i as usize][1] = names.slots[r as usize][1];
    }
    if std::env::var_os("SS_DEBUG").is_some() {
        eprintln!(
            "links: alloc {t1:.3}s, unions {:.3}s, names {:.3}s, {} nodes, {} slots",
            t2 - t1,
            t0.elapsed().as_secs_f64() - t2,
            occupied.len(),
            names.slots.len()
        );
    }
    names
}
