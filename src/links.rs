//! Linked IDs: union-find over every link, each person named by the byte-wise
//! smallest of their IDs.

use std::cmp::Ordering;

use crate::hash::mix;
use crate::key;

/// Open addressing from a linked user key to its person's name: one 16-byte slot
/// (key, name) per entry, so a lookup touches a single cache line. Key 0 is empty.
///
/// A bitset over the keys' hashes, small enough for the last-level cache, holds a
/// bit for every key named by another key: a key whose bit is clear names itself,
/// without reading the table (most keys).
pub struct Names {
    slots: &'static mut [[u64; 2]],
    size: usize,
    filter: &'static mut [u64],
    /// 64 - log2(filter bits); 0 while there is no filter (during the build).
    shift: u32,
}

impl Names {
    /// A table of exactly `size` slots (any size: home slots by multiply-shift).
    fn with_slots(size: usize) -> Self {
        let size = size.max(16);
        assert!(size < u32::MAX as usize, "name table too large");
        Self {
            slots: crate::arena::zeroed_slice(size),
            size,
            filter: &mut [],
            shift: 0,
        }
    }

    #[inline]
    fn home(&self, key: u64) -> usize {
        crate::hash::bucket(mix(key), self.size)
    }

    /// The filter word and bit of a key's hash.
    #[inline]
    fn filter_bit(&self, h: u64) -> (usize, u32) {
        let b = (h >> self.shift) as usize;
        (b / 64, (b % 64) as u32)
    }

    /// False when the key surely names itself.
    #[inline]
    fn maybe_renamed(&self, h: u64) -> bool {
        if self.shift == 0 {
            return true;
        }
        let (w, b) = self.filter_bit(h);
        // SAFETY: the word index is below filter.len() = 2^(64 - shift) / 64.
        (unsafe { *self.filter.get_unchecked(w) } >> b) & 1 != 0
    }

    /// The person's name for a user key: itself if it appears in no link.
    #[inline]
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn get(&self, user: u64) -> u64 {
        let h = mix(user);
        if !self.maybe_renamed(h) {
            return user;
        }
        self.probe(user, h)
    }

    /// `get` for a key whose filter bit is known to be set (a deferred lookup):
    /// straight to the table.
    #[inline]
    pub fn get_flagged(&self, user: u64) -> u64 {
        self.probe(user, mix(user))
    }

    #[inline(always)]
    fn probe(&self, user: u64, h: u64) -> u64 {
        let mut i = crate::hash::bucket(h, self.size);
        loop {
            let [k, name] = self.slots[i];
            if k == user {
                return name;
            }
            if k == 0 {
                return user;
            }
            i += 1;
            if i == self.size {
                i = 0;
            }
        }
    }

    /// How many regions `region` cuts the table into: about 256 KiB each, so one
    /// region's slots stay in the L2 cache while its lookups run.
    pub fn regions(&self) -> usize {
        (self.size * 16 / (256 << 10)).clamp(1, 4096)
    }

    /// The region of the table where `user`'s lookup starts, or None when the
    /// filter says it names itself (no lookup needed).
    #[inline]
    pub fn region(&self, user: u64, regions: usize) -> Option<usize> {
        let h = mix(user);
        if !self.maybe_renamed(h) {
            return None;
        }
        Some(crate::hash::bucket(h, self.size) * regions / self.size)
    }

    /// First of two prefetch steps: the filter word.
    #[inline]
    pub fn prefetch_filter(&self, user: u64) {
        if self.shift != 0 {
            prefetch(self.filter.as_ptr().wrapping_add(self.filter_bit(mix(user)).0));
        }
    }

    /// Only the slot (while building).
    #[inline]
    fn prefetch_slot(&self, key: u64) {
        prefetch(self.slots.as_ptr().wrapping_add(self.home(key)));
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

/// Union-find with path halving over dense nodes: `node[i]` is (parent, size).
#[inline]
fn find(node: &mut [[u32; 2]], mut x: u32) -> u32 {
    while node[x as usize][0] != x {
        let grand = node[node[x as usize][0] as usize][0];
        node[x as usize][0] = grand;
        x = grand;
    }
    x
}

/// Union-find over every linked ID. The big table maps a key to a dense node
/// number (its slot's second word during the build); the unions run on a small
/// dense array; a last sequential pass writes each slot's name.
///
/// Every random access is software-pipelined: the table slots of pair i + 2L are
/// prefetched, pair i + L gets its node numbers (and its nodes are prefetched),
/// and pair i is united.
pub fn build(pairs: &[Vec<(u64, u64)>]) -> Names {
    const L: usize = 16;
    let t0 = std::time::Instant::now();
    let total: usize = pairs.iter().map(Vec::len).sum();
    // At most 2 nodes per pair, about 1.4 in practice: 5 slots per pair keeps the
    // load near 0.3 and the table never full.
    let names = Names::with_slots(total * 5 + 64);
    let size = names.size;
    let mut keys: Vec<u64> = Vec::with_capacity(2 * total + 16);
    let mut node: Vec<[u32; 2]> = Vec::with_capacity(2 * total + 16);
    let t1 = t0.elapsed().as_secs_f64();

    let node_of = |slots: &mut [[u64; 2]], keys: &mut Vec<u64>, node: &mut Vec<[u32; 2]>, key: u64| -> u32 {
        let mut i = crate::hash::bucket(mix(key), size);
        loop {
            let k = slots[i][0];
            if k == key {
                return slots[i][1] as u32;
            }
            if k == 0 {
                let n = keys.len() as u32;
                slots[i] = [key, n as u64];
                keys.push(key);
                node.push([n, 1]);
                return n;
            }
            i += 1;
            if i == size {
                i = 0;
            }
        }
    };
    let mut far = pairs.iter().flatten();
    let mut near = pairs.iter().flatten();
    let mut ring = [(0u32, 0u32); L];
    for &(x, y) in far.by_ref().take(2 * L) {
        names.prefetch_slot(x);
        names.prefetch_slot(y);
    }
    let prefetch_node = |node: &Vec<[u32; 2]>, n: u32| prefetch(node.as_ptr().wrapping_add(n as usize));
    for (k, &(a, b)) in near.by_ref().take(L).enumerate() {
        let na = node_of(names.slots, &mut keys, &mut node, a);
        let nb = node_of(names.slots, &mut keys, &mut node, b);
        ring[k] = (na, nb);
    }
    for i in 0..total {
        let (na, nb) = ring[i % L];
        if let Some(&(x, y)) = far.next() {
            names.prefetch_slot(x);
            names.prefetch_slot(y);
        }
        let ra = find(&mut node, na);
        let rb = find(&mut node, nb);
        if ra != rb {
            let (big, small) = if node[ra as usize][1] >= node[rb as usize][1] { (ra, rb) } else { (rb, ra) };
            node[small as usize][0] = big;
            node[big as usize][1] += node[small as usize][1];
        }
        if let Some(&(a, b)) = near.next() {
            let na = node_of(names.slots, &mut keys, &mut node, a);
            let nb = node_of(names.slots, &mut keys, &mut node, b);
            prefetch_node(&node, na);
            prefetch_node(&node, nb);
            ring[i % L] = (na, nb);
        }
    }
    let t2 = t0.elapsed().as_secs_f64();
    // Each node's root (prefetching the parent of node i + L), then the smallest
    // key of each root, then each node's name, then every slot's.
    let n = keys.len();
    for i in 0..n {
        if let Some(ahead) = node.get(i + L) {
            prefetch_node(&node, ahead[0]);
        }
        let r = find(&mut node, i as u32);
        node[i][0] = r;
    }
    let mut best = vec![0u64; n];
    for i in 0..n {
        if let Some(ahead) = node.get(i + L) {
            prefetch(best.as_ptr().wrapping_add(ahead[0] as usize));
        }
        let r = node[i][0] as usize;
        if best[r] == 0 || key::cmp(keys[i], best[r]) == Ordering::Less {
            best[r] = keys[i];
        }
    }
    // The filter: SS_FILTER bits per node (0 = none).
    let per_node = std::env::var("SS_FILTER").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(8);
    let mut filter: &'static mut [u64] = &mut [];
    let mut shift = 0;
    if per_node > 0 {
        let bits = (n * per_node).next_power_of_two().max(64);
        filter = crate::arena::zeroed_slice(bits / 64);
        shift = 64 - bits.trailing_zeros();
    }
    let mut name_of = keys; // reused: each node's key becomes its name
    for i in 0..n {
        if let Some(ahead) = node.get(i + L) {
            prefetch(best.as_ptr().wrapping_add(ahead[0] as usize));
        }
        let key = name_of[i];
        let name = best[node[i][0] as usize];
        name_of[i] = name;
        if shift != 0 && name != key {
            let b = (mix(key) >> shift) as usize;
            filter[b / 64] |= 1 << (b % 64);
        }
    }
    let slots = &mut *names.slots;
    for j in 0..slots.len() {
        if let Some(ahead) = slots.get(j + L) {
            if ahead[0] != 0 {
                prefetch(name_of.as_ptr().wrapping_add(ahead[1] as usize));
            }
        }
        let slot = &mut slots[j];
        if slot[0] != 0 {
            slot[1] = name_of[slot[1] as usize];
        }
    }
    let mut names = names;
    names.filter = filter;
    names.shift = shift;
    if std::env::var_os("SS_DEBUG").is_some() {
        eprintln!(
            "links: alloc {t1:.3}s, unions {:.3}s, names {:.3}s, {} nodes, {} slots",
            t2 - t1,
            t0.elapsed().as_secs_f64() - t2,
            n,
            names.slots.len()
        );
    }
    names
}

/// The table of `build`, built on `threads` threads, lock-free: the slots are the
/// union-find nodes. During the build a slot's second word is its parent's slot + 1
/// (0 = a root). A union hangs, by compare-and-swap, the root whose key sorts last
/// under the other root, so the root of a component is always its smallest key (its
/// name) whatever the interleaving; finds halve paths with plain stores (a parent
/// pointer only ever moves to an ancestor). After the unions every node points
/// straight at its root, then every second word becomes the name.
pub fn build_parallel(pairs: &[Vec<(u64, u64)>], threads: usize) -> Names {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
    const TASK: usize = 1 << 14;
    const L: usize = 16;
    let total: usize = pairs.iter().map(Vec::len).sum();
    // slots per link: at most 2 nodes per link (about 1.4 in practice)
    let per_link = std::env::var("SS_NAME_SLOTS").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(3).clamp(3, 16);
    let mut names = Names::with_slots(total * per_link + 64);
    let size = names.size;
    // SAFETY: AtomicU64 has the layout of u64; only this view touches the slots until the end.
    let slots: &[[AtomicU64; 2]] = unsafe { std::slice::from_raw_parts(names.slots.as_mut_ptr() as *const [AtomicU64; 2], size) };
    let home = |key: u64| crate::hash::bucket(mix(key), size);
    let insert = |key: u64, inserted: &mut usize| -> u32 {
        let mut i = home(key);
        loop {
            let cur = slots[i][0].load(Relaxed);
            if cur == key {
                return i as u32;
            }
            if cur == 0 {
                match slots[i][0].compare_exchange(0, key, Relaxed, Relaxed) {
                    Ok(_) => {
                        *inserted += 1;
                        return i as u32;
                    }
                    Err(now) if now == key => return i as u32,
                    Err(_) => {}
                }
            }
            i += 1;
            if i == size {
                i = 0;
            }
        }
    };
    let parent = |x: usize| match slots[x][1].load(Relaxed) {
        0 => x,
        w => (w - 1) as usize,
    };
    let find = |mut x: usize| loop {
        let p = parent(x);
        if p == x {
            return x;
        }
        let g = parent(p);
        if g != p {
            slots[x][1].store(g as u64 + 1, Relaxed); // g is an ancestor of x
        }
        x = g;
    };
    let tasks: Vec<(usize, usize, usize)> = pairs
        .iter()
        .enumerate()
        .flat_map(|(v, p)| (0..p.len()).step_by(TASK).map(move |s| (v, s, (s + TASK).min(p.len()))))
        .collect();
    let threads = threads.clamp(1, tasks.len().max(1));

    let clock = std::time::Instant::now();
    let mut laps = Vec::new();
    // 1. every endpoint into the table, prefetched L pairs ahead
    let next = AtomicUsize::new(0);
    let mut nodes = 0usize;
    let mut slot_pairs: Vec<Vec<(u32, u32)>> = Vec::new();
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (tasks, next, insert) = (&tasks, &next, &insert);
                s.spawn(move || {
                    let mut out = Vec::new();
                    let mut inserted = 0usize;
                    loop {
                        let t = next.fetch_add(1, Relaxed);
                        let Some(&(v, start, end)) = tasks.get(t) else { break };
                        let p = &pairs[v][start..end];
                        let mut sp = Vec::with_capacity(p.len());
                        for (j, &(a, b)) in p.iter().enumerate() {
                            if let Some(&(x, y)) = p.get(j + L) {
                                prefetch(slots.as_ptr().wrapping_add(home(x)));
                                prefetch(slots.as_ptr().wrapping_add(home(y)));
                            }
                            sp.push((insert(a, &mut inserted), insert(b, &mut inserted)));
                        }
                        out.push(sp);
                    }
                    (out, inserted)
                })
            })
            .collect();
        for h in handles {
            let (out, inserted) = h.join().unwrap();
            slot_pairs.extend(out);
            nodes += inserted;
        }
    });

    laps.push(clock.elapsed().as_secs_f64());
    // 2. unions, the slots of pair j + L prefetched
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads {
            let (slot_pairs, next, find) = (&slot_pairs, &next, &find);
            s.spawn(move || loop {
                let t = next.fetch_add(1, Relaxed);
                let Some(sp) = slot_pairs.get(t) else { break };
                for (j, &(a, b)) in sp.iter().enumerate() {
                    if let Some(&(x, y)) = sp.get(j + L) {
                        prefetch(slots.as_ptr().wrapping_add(x as usize));
                        prefetch(slots.as_ptr().wrapping_add(y as usize));
                    }
                    loop {
                        let (ra, rb) = (find(a as usize), find(b as usize));
                        if ra == rb {
                            break;
                        }
                        let (ka, kb) = (slots[ra][0].load(Relaxed), slots[rb][0].load(Relaxed));
                        let (child, top) = if key::cmp(ka, kb) == Ordering::Greater { (ra, rb) } else { (rb, ra) };
                        if slots[child][1].compare_exchange(0, top as u64 + 1, Relaxed, Relaxed).is_ok() {
                            break;
                        }
                    }
                }
            });
        }
    });

    laps.push(clock.elapsed().as_secs_f64());
    // 3. every node straight to its root; 4. every second word becomes the name, and
    //    each renamed key gets its filter bit
    let per_node = std::env::var("SS_FILTER").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(8).max(1);
    let bits = (nodes * per_node).next_power_of_two().max(64);
    let filter: &'static mut [u64] = crate::arena::zeroed_slice(bits / 64);
    let shift = 64 - bits.trailing_zeros();
    // SAFETY: as for the slots.
    let filter_atomic: &[AtomicU64] = unsafe { std::slice::from_raw_parts(filter.as_mut_ptr() as *const AtomicU64, filter.len()) };
    let ranges: Vec<(usize, usize)> = (0..size).step_by(1 << 16).map(|s| (s, (s + (1 << 16)).min(size))).collect();
    for step in 0..2 {
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads {
                let (ranges, next, find) = (&ranges, &next, &find);
                s.spawn(move || loop {
                    let r = next.fetch_add(1, Relaxed);
                    let Some(&(start, end)) = ranges.get(r) else { break };
                    for i in start..end {
                        let key = slots[i][0].load(Relaxed);
                        if key == 0 {
                            continue;
                        }
                        if step == 0 {
                            let root = find(i);
                            if root != i {
                                slots[i][1].store(root as u64 + 1, Relaxed);
                            }
                        } else {
                            let name = match slots[i][1].load(Relaxed) {
                                0 => key,
                                w => slots[(w - 1) as usize][0].load(Relaxed),
                            };
                            slots[i][1].store(name, Relaxed);
                            if name != key {
                                let b = (mix(key) >> shift) as usize;
                                filter_atomic[b / 64].fetch_or(1 << (b % 64), Relaxed);
                            }
                        }
                    }
                });
            }
        });
        laps.push(clock.elapsed().as_secs_f64());
    }
    if crate::stats::enabled() {
        eprintln!("           names: {nodes} nodes, {size} slots; inserts {:.3}s, unions {:.3}s, roots {:.3}s, names {:.3}s", laps[0], laps[1] - laps[0], laps[2] - laps[1], laps[3] - laps[2]);
    }
    names.filter = filter;
    names.shift = shift;
    names
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

    /// The parallel build gives every key the same name as the serial one.
    #[test]
    fn parallel_build_matches_serial() {
        for round in 0..40u64 {
            let mut s = 0x9e37_79b9_7f4a_7c15 ^ round.wrapping_mul(0x1234_5678_9abc_def1);
            let n = 20 + round as usize * 53;
            let ids: Vec<u64> = (0..n)
                .map(|i| match rng(&mut s) % 7 {
                    0 => key::key(format!("U{i}").as_bytes()),
                    1 => key::key(format!("u{i} ").as_bytes()),
                    2 => key::key(format!("long_user_identifier_{i}").as_bytes()),
                    _ => key::key(format!("u{}", rng(&mut s) % 1_000_000).as_bytes()),
                })
                .collect();
            let mut pairs: Vec<Vec<(u64, u64)>> = vec![Vec::new(); 4];
            for k in 0..(n * (1 + round as usize % 3)) {
                let a = ids[(rng(&mut s) % n as u64) as usize];
                let b = if k % 7 == 0 { a } else { ids[(rng(&mut s) % n as u64) as usize] };
                pairs[k % 4].push((a, b));
            }
            for k in 1..n.min(300) {
                pairs[k % 4].push((ids[k - 1], ids[k])); // a long chain
            }
            let serial = build(&pairs);
            let parallel = build_parallel(&pairs, 1 + round as usize % 8);
            for &id in &ids {
                assert_eq!(parallel.get(id), serial.get(id), "round {round}");
            }
            for x in [key::key(b"u999999999"), key::key(b"zz")] {
                assert_eq!(parallel.get(x), serial.get(x));
            }
        }
    }
}
