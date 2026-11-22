//! Dedup of event copies, then per person: sessions, credit, assists and the daily
//! aggregates.

use std::cmp::Ordering;

use crate::arena::Scatter;
use crate::events::{category_of, price_of, ts_of, EventCopy, C_SURVIVES, C_TS, PURCHASE, SURVIVES, WEEK};
use crate::hash::{bucket, mix};
use crate::key;
use crate::links::Names;
use crate::ts::DAY;

pub const GAP: i64 = 1_800_000;
pub const NO_CATEGORY: u16 = u16::MAX;

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct PersonEvent {
    pub person: u64,
    /// As in EventCopy (see events::pack).
    pub packed: u64,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct PersonClick {
    pub person: u64,
    pub click_id: u64,
    /// Relative to Context::base.
    pub ts: u32,
    pub campaign: u32,
}

// Clicks travel as 3-word records (arena::Words3) laid out like PersonClick.
const _: () = assert!(std::mem::size_of::<PersonClick>() == 24 && std::mem::align_of::<PersonClick>() == 8);
#[cfg(not(target_endian = "little"))]
compile_error!("PersonClick words assume a little-endian layout");

impl PersonClick {
    #[inline(always)]
    pub fn words(self) -> [u64; 3] {
        [self.person, self.click_id, self.ts as u64 | (self.campaign as u64) << 32]
    }

    /// The clicks of a slice of `words` records.
    #[inline]
    pub fn of_words(w: &[[u64; 3]]) -> &[PersonClick] {
        // SAFETY: same size and alignment, and `words` follows the repr(C) layout
        // (ts in the low half of the third word, little-endian).
        unsafe { std::slice::from_raw_parts(w.as_ptr() as *const PersonClick, w.len()) }
    }
}

/// A dedup table reused across partitions without clearing it. A slot packs the
/// kept copy's position (bits 0..32), 24 bits of its event_id's hash (32..56) and
/// the partition's generation (56..64): a slot of another generation is empty.
#[derive(Default)]
pub struct Table {
    slots: Vec<u64>,
    generation: u64,
    /// One bit per copy of the partition: set when another copy wins.
    lost: Vec<u64>,
    /// The dense number of each slice's first copy.
    first: Vec<u32>,
}

/// Keeps, per event_id, the copy with the smallest ingest_ts (copies tied on it are
/// identical) and sends the kept copies that survive to their person's partition.
/// This event_id partition's copies come as compact and full slices (see
/// events::Copies); `base` is Context::base.
///
/// The first pass inserts every copy into a small table and marks the copies that
/// lose; the second reads the copies again in order, skipping the losers.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
pub fn dedup(
    compact: &[&[[u64; 3]]],
    full: &[&[EventCopy]],
    base: i64,
    names: &Names,
    table: &mut Table,
    kept: &mut Vec<PersonEvent>,
    out: &mut Scatter<PersonEvent>,
    deferred: &mut Scatter<PersonEvent>,
) {
    let t = crate::prof::start();
    let nc = compact.len();
    let slices = nc + full.len();
    let n: usize = compact.iter().map(|p| p.len()).sum::<usize>() + full.iter().map(|p| p.len()).sum::<usize>();
    if n == 0 {
        return;
    }
    assert!(n < u32::MAX as usize, "partition too large");
    // A position packs the slice index (compact slices first) in its high bits and
    // the offset in the rest; `first[k]` is the dense number of slice k's first copy.
    let slice_bits = usize::BITS - (slices.max(2) - 1).leading_zeros();
    let shift = 32 - slice_bits;
    assert!(
        compact.iter().map(|p| p.len()).chain(full.iter().map(|p| p.len())).all(|l| (l as u64) < (1u64 << shift)),
        "partition too large"
    );
    let offset_mask = (1u64 << shift) - 1;
    // (event_id, ingest_ts) of the copy at a position
    let copy_at = |at: u64| -> (u64, i64) {
        let (k, o) = ((at >> shift) as usize, (at & offset_mask) as usize);
        if k < nc {
            let c = &compact[k][o];
            (c[0], base + ((c[2] >> 32) as u32 as i32 as i64) * 1000)
        } else {
            let c = &full[k - nc][o];
            (c.event_id, c.ingest)
        }
    };
    table.first.clear();
    let mut acc = 0u32;
    for len in compact.iter().map(|p| p.len()).chain(full.iter().map(|p| p.len())) {
        table.first.push(acc);
        acc += len as u32;
    }
    let first = &table.first;
    let dense = |at: u64| -> usize { first[(at >> shift) as usize] as usize + (at & offset_mask) as usize };

    let size = (n * 2).next_power_of_two();
    let mask = size - 1;
    if table.slots.len() < size {
        table.slots.resize(size, 0);
    }
    table.generation += 1;
    if table.generation == 256 {
        // the 8-bit generation wrapped: start over from a clean table
        table.slots.iter_mut().for_each(|s| *s = 0);
        table.generation = 1;
    }
    let generation = table.generation << 56;
    table.lost.clear();
    table.lost.resize(n.div_ceil(64), 0);
    let lost = &mut table.lost;
    let slots = &mut table.slots[..size];
    let mut insert = |at: u64, event_id: u64, ingest: i64| {
        let h = mix(event_id);
        let tag = (h >> 40) << 32; // 24 bits, above the position
        let mut i = h as usize & mask;
        loop {
            let s = slots[i];
            if s >> 56 << 56 != generation {
                slots[i] = generation | tag | at;
                return;
            }
            if s & (0xff_ffff << 32) == tag {
                let old = s & 0xffff_ffff;
                let (other_id, other_ingest) = copy_at(old);
                if other_id == event_id {
                    // A duplicate (rare): the smaller ingest_ts wins; ties are identical copies.
                    let loser = if ingest < other_ingest {
                        slots[i] = generation | tag | at;
                        old
                    } else {
                        at
                    };
                    let d = dense(loser);
                    lost[d / 64] |= 1 << (d % 64);
                    return;
                }
            }
            i = (i + 1) & mask;
        }
    };
    for (k, part) in compact.iter().enumerate() {
        for (o, c) in part.iter().enumerate() {
            insert(((k as u64) << shift) | o as u64, c[0], base + ((c[2] >> 32) as u32 as i32 as i64) * 1000);
        }
    }
    for (k, part) in full.iter().enumerate() {
        for (o, c) in part.iter().enumerate() {
            insert((((nc + k) as u64) << shift) | o as u64, c.event_id, c.ingest);
        }
    }
    let t = crate::prof::lap(crate::prof::DEDUP_INSERT, t);
    // The kept copies that survive, in order, then their names.
    kept.clear();
    for (k, part) in compact.iter().enumerate() {
        let start = first[k] as usize;
        for (o, c) in part.iter().enumerate() {
            let d = start + o;
            if c[2] & C_SURVIVES != 0 && (lost[d / 64] >> (d % 64)) & 1 == 0 {
                kept.push(PersonEvent { person: c[1], packed: (c[2] & C_TS) << 32 });
            }
        }
    }
    for (k, part) in full.iter().enumerate() {
        let start = first[nc + k] as usize;
        for (o, c) in part.iter().enumerate() {
            let d = start + o;
            if c.packed & SURVIVES != 0 && (lost[d / 64] >> (d % 64)) & 1 == 0 {
                kept.push(PersonEvent { person: c.user, packed: c.packed });
            }
        }
    }
    let t = crate::prof::lap(crate::prof::DEDUP_KEPT, t);
    // A user the filter clears is its own person and goes straight to its
    // partition; the others wait in `deferred`, by names-table region, for
    // `resolve_deferred` (their lookups then hit a region in cache, not DRAM).
    let regions = deferred.parts();
    for e in kept.iter().take(LAG) {
        names.prefetch_filter(e.person);
    }
    for i in 0..kept.len() {
        if let Some(ahead) = kept.get(i + LAG) {
            names.prefetch_filter(ahead.person);
        }
        let e = kept[i];
        match names.region(e.person, regions) {
            None => out.push_wc(bucket(mix(e.person), out.parts()), e),
            Some(r) => deferred.push_wc(r, e),
        }
    }
    crate::prof::lap(crate::prof::DEDUP_ROUTE, t);
}

/// The deferred clicks of one names-table region (one slice per parsing thread):
/// their persons, then their person partitions.
#[inline(never)]
pub fn resolve_deferred_clicks<'a>(clicks: impl Iterator<Item = &'a [[u64; 3]]>, names: &Names, out: &mut crate::arena::Words3) {
    for part in clicks {
        for c in PersonClick::of_words(part) {
            let c = PersonClick { person: names.get_flagged(c.person), ..*c };
            out.push(bucket(mix(c.person), out.parts()), c.words());
        }
    }
}

/// The deferred events of one names-table region (one slice per dedup thread):
/// their persons, then their person partitions.
#[inline(never)]
pub fn resolve_deferred<'a>(events: impl Iterator<Item = &'a [PersonEvent]>, names: &Names, out: &mut Scatter<PersonEvent>) {
    for part in events {
        for &e in part {
            let e = PersonEvent { person: names.get_flagged(e.person), packed: e.packed };
            out.push_wc(bucket(mix(e.person), out.parts()), e);
        }
    }
}

const LAG: usize = 24;

#[derive(Clone, Copy)]
pub struct Shape {
    /// Context::base: ts values in records are relative to it.
    pub base: i64,
    pub first_day: i64,
    pub days: usize,
    pub campaigns: usize,
}

/// One thread's share of the output.
pub struct Acc {
    pub user: Vec<u64>,
    pub day: Vec<i32>,
    pub sessions: Vec<i64>,
    pub events: Vec<i64>,
    pub revenue: Vec<i64>,
    pub top: Vec<u16>,
    /// Day-major, `[day * (campaigns + 1) + campaign + 1]` (index 0 of a row is
    /// `organic`): credited sessions, purchasing sessions and revenue, then assisted
    /// sessions apart. A session's updates all land in its day's rows, which stay
    /// in cache. `add_stats` merges them.
    pub credited: Vec<(u32, u32, i64)>,
    pub assists: Vec<u32>,
    shape: Shape,
    count: Vec<u32>,
    pos: Vec<u32>,
    active: Vec<u32>,
    categories: Vec<(u16, i64)>,
    evs: Vec<PersonEvent>,
    cls: Vec<PersonClick>,
    /// Radix sort scratch, the persons by rank and the rank table.
    evs_tmp: Vec<PersonEvent>,
    cls_tmp: Vec<PersonClick>,
    persons: Vec<u64>,
    ranks: Ranks,
    /// Per-partition 30-minute buckets: the table, the buckets and the purchases.
    buckets: Buckets,
    aggs: Vec<Agg>,
    aggs_tmp: Vec<Agg>,
    purchases: Vec<PersonEvent>,
}

/// The events of one person in one 30-minute bucket of ts: how many, the first
/// and the last ts. Two events of one bucket are less than 30 minutes apart, so
/// they always share a session; a session breaks between two buckets exactly when
/// the next one's first ts is more than 30 minutes after the previous one's last.
#[derive(Clone, Copy, Default)]
pub struct Agg {
    /// The person, then its rank once ranked.
    pub person: u64,
    pub bucket: u32,
    pub count: u32,
    pub min: u32,
    pub max: u32,
}

const BUCKET: u32 = 1_800_000;

/// (person, bucket) -> index in `aggs`: open addressing, generation-stamped.
#[derive(Default)]
struct Buckets {
    /// person, bucket << 32 | generation, index
    slots: Vec<(u64, u64, u32)>,
    generation: u32,
}

impl Buckets {
    fn reset(&mut self) {
        if self.slots.is_empty() {
            self.slots = vec![(0, 0, 0); 1 << 14];
        }
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.slots.iter_mut().for_each(|s| *s = (0, 0, 0));
            self.generation = 1;
        }
    }

    #[inline(always)]
    fn home(person: u64, bucket: u32, mask: usize) -> usize {
        ((person ^ (bucket as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)).wrapping_mul(0xff51_afd7_ed55_8ccd) >> 40) as usize & mask
    }

    /// Adds an event at ts (relative) to its bucket.
    #[inline(always)]
    fn add(&mut self, person: u64, ts: u32, aggs: &mut Vec<Agg>) {
        let bucket = ts / BUCKET;
        let tag = (bucket as u64) << 32 | self.generation as u64;
        let mask = self.slots.len() - 1;
        let mut i = Self::home(person, bucket, mask);
        loop {
            // SAFETY: i <= mask < slots.len().
            let slot = unsafe { self.slots.get_unchecked_mut(i) };
            if slot.1 as u32 != self.generation {
                *slot = (person, tag, aggs.len() as u32);
                aggs.push(Agg { person, bucket, count: 1, min: ts, max: ts });
                if aggs.len() * 2 > self.slots.len() {
                    self.grow(aggs);
                }
                return;
            }
            if slot.0 == person && slot.1 == tag {
                // SAFETY: the index came from aggs.len() at insertion.
                let a = unsafe { aggs.get_unchecked_mut(slot.2 as usize) };
                a.count += 1;
                a.min = a.min.min(ts);
                a.max = a.max.max(ts);
                return;
            }
            i = (i + 1) & mask;
        }
    }

    fn grow(&mut self, aggs: &[Agg]) {
        let size = self.slots.len() * 2;
        self.slots = vec![(0, 0, 0); size];
        self.generation = 1;
        let mask = size - 1;
        for (idx, a) in aggs.iter().enumerate() {
            let mut i = Self::home(a.person, a.bucket, mask);
            while self.slots[i].1 as u32 == self.generation {
                i = (i + 1) & mask;
            }
            self.slots[i] = (a.person, (a.bucket as u64) << 32 | 1, idx as u32);
        }
    }
}

static OLD_SESSIONS: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| std::env::var("SS_SESSIONS").as_deref() == Ok("old"));

impl Acc {
    /// `rows`: about how many user_daily rows this thread will produce.
    pub fn new(shape: Shape, rows: usize) -> Self {
        use crate::arena::huge_vec;
        Self {
            user: huge_vec(rows),
            day: huge_vec(rows),
            sessions: huge_vec(rows),
            events: huge_vec(rows),
            revenue: huge_vec(rows),
            top: huge_vec(rows),
            credited: vec![(0, 0, 0); (shape.campaigns + 1) * shape.days],
            assists: vec![0; (shape.campaigns + 1) * shape.days],
            shape,
            count: vec![0; shape.campaigns],
            pos: vec![u32::MAX; shape.campaigns],
            active: Vec::new(),
            categories: Vec::new(),
            evs: Vec::new(),
            cls: Vec::new(),
            evs_tmp: Vec::new(),
            cls_tmp: Vec::new(),
            persons: Vec::new(),
            ranks: Ranks::default(),
            buckets: Buckets::default(),
            aggs: Vec::new(),
            aggs_tmp: Vec::new(),
            purchases: Vec::new(),
        }
    }

    /// `[(campaign + 1) * days + day]`: credited sessions, purchasing sessions,
    /// revenue, assisted sessions, added into `total`. Index 0 is `organic`.
    pub fn add_stats(&self, total: &mut [[i64; 4]]) {
        let (days, width) = (self.shape.days, self.shape.campaigns + 1);
        for d in 0..days {
            for c in 0..width {
                let (sessions, purchasing, revenue) = self.credited[d * width + c];
                let t = &mut total[c * days + d];
                t[0] += sessions as i64;
                t[1] += purchasing as i64;
                t[2] += revenue;
                t[3] += self.assists[d * width + c] as i64;
            }
        }
    }

    /// Processes one person partition, given as one slice per thread.
    ///
    /// Each person gets a dense rank (in order of first event); events, then
    /// clicks, are radix-sorted by (rank, ts), so each person's events and clicks
    /// form one run in ts order. Clicks of a person without events are dropped.
    #[inline(never)]
    pub fn partition<'a>(
        &mut self,
        evs: impl Iterator<Item = &'a [PersonEvent]>,
        cls: impl Iterator<Item = &'a [PersonClick]>,
    ) {
        if !*OLD_SESSIONS {
            return self.partition_buckets(evs, cls);
        }
        let mut e = std::mem::take(&mut self.evs);
        let mut c = std::mem::take(&mut self.cls);
        let mut persons = std::mem::take(&mut self.persons);
        e.clear();
        c.clear();
        persons.clear();
        self.ranks.reset();
        let t = crate::prof::start();
        for part in evs {
            for x in part {
                let rank = self.ranks.rank(x.person, &mut persons);
                e.push(PersonEvent { person: rank as u64, packed: x.packed });
            }
        }
        for part in cls {
            for x in part {
                if let Some(rank) = self.ranks.find(x.person) {
                    c.push(PersonClick { person: rank as u64, ..*x });
                }
            }
        }
        let t = crate::prof::lap(crate::prof::SESS_RANK, t);
        let rank_bits = u64::BITS - (persons.len() as u64).leading_zeros();
        // ts sits in the high 32 bits of packed: the key is (rank, ts).
        radix_sort(&mut e, &mut self.evs_tmp, 32 + rank_bits, |x| x.person << 32 | x.packed >> 32);
        radix_sort(&mut c, &mut self.cls_tmp, 32 + rank_bits, |x| x.person << 32 | x.ts as u64);
        let t = crate::prof::lap(crate::prof::SESS_SORT, t);
        let (mut i, mut ci) = (0usize, 0usize);
        while i < e.len() {
            let rank = e[i].person;
            let mut j = i + 1;
            while j < e.len() && e[j].person == rank {
                j += 1;
            }
            while ci < c.len() && c[ci].person < rank {
                ci += 1;
            }
            let mut cj = ci;
            while cj < c.len() && c[cj].person == rank {
                cj += 1;
            }
            self.person(persons[rank as usize], &e[i..j], &c[ci..cj]);
            i = j;
            ci = cj;
        }
        crate::prof::lap(crate::prof::SESS_PERSON, t);
        self.evs = e;
        self.cls = c;
        self.persons = persons;
    }

    #[inline]
    fn enter(&mut self, campaign: u32) {
        let c = campaign as usize;
        if self.count[c] == 0 {
            self.pos[c] = self.active.len() as u32;
            self.active.push(campaign);
        }
        self.count[c] += 1;
    }

    #[inline]
    fn leave(&mut self, campaign: u32) {
        let c = campaign as usize;
        self.count[c] -= 1;
        if self.count[c] == 0 {
            let p = self.pos[c] as usize;
            let last = self.active.pop().unwrap();
            if last != campaign {
                self.active[p] = last;
                self.pos[last as usize] = p as u32;
            }
            self.pos[c] = u32::MAX;
        }
    }

    fn flush(&mut self, person: u64, day: i64, sessions: i64, events: i64, revenue: i64) {
        let mut top = NO_CATEGORY;
        let mut best = -1i64;
        for &(category, sum) in &self.categories {
            if sum > best || (sum == best && category < top) {
                best = sum;
                top = category;
            }
        }
        self.user.push(person);
        self.day.push(day as i32);
        self.sessions.push(sessions);
        self.events.push(events);
        self.revenue.push(revenue);
        self.top.push(top);
    }

    /// `partition` without sorting the events: they are counted into (person,
    /// 30-minute bucket) aggregates; only the aggregates (ranked, then sorted by
    /// (rank, bucket)), the purchases and the clicks are sorted.
    fn partition_buckets<'a>(&mut self, evs: impl Iterator<Item = &'a [PersonEvent]>, cls: impl Iterator<Item = &'a [PersonClick]>) {
        let mut aggs = std::mem::take(&mut self.aggs);
        let mut purchases = std::mem::take(&mut self.purchases);
        let mut c = std::mem::take(&mut self.cls);
        let mut persons = std::mem::take(&mut self.persons);
        aggs.clear();
        purchases.clear();
        c.clear();
        persons.clear();
        self.buckets.reset();
        self.ranks.reset();
        let t = crate::prof::start();
        for part in evs {
            for x in part {
                self.buckets.add(x.person, (x.packed >> 32) as u32, &mut aggs);
                if x.packed & PURCHASE != 0 {
                    purchases.push(*x);
                }
            }
        }
        // ranks in order of first bucket; purchases and clicks by the same ranks
        for a in aggs.iter_mut() {
            a.person = self.ranks.rank(a.person, &mut persons) as u64;
        }
        for p in purchases.iter_mut() {
            p.person = self.ranks.find(p.person).expect("a purchase has its bucket") as u64;
        }
        for part in cls {
            for x in part {
                if let Some(rank) = self.ranks.find(x.person) {
                    c.push(PersonClick { person: rank as u64, ..*x });
                }
            }
        }
        let t = crate::prof::lap(crate::prof::SESS_RANK, t);
        let rank_bits = u64::BITS - (persons.len() as u64).leading_zeros();
        radix_sort(&mut aggs, &mut self.aggs_tmp, 16 + rank_bits, |a| a.person << 16 | a.bucket as u64);
        radix_sort(&mut purchases, &mut self.evs_tmp, 32 + rank_bits, |x| x.person << 32 | x.packed >> 32);
        radix_sort(&mut c, &mut self.cls_tmp, 32 + rank_bits, |x| x.person << 32 | x.ts as u64);
        let t = crate::prof::lap(crate::prof::SESS_SORT, t);
        let (mut i, mut pi, mut ci) = (0usize, 0usize, 0usize);
        while i < aggs.len() {
            let rank = aggs[i].person;
            let mut j = i + 1;
            while j < aggs.len() && aggs[j].person == rank {
                j += 1;
            }
            let mut pj = pi;
            while pj < purchases.len() && purchases[pj].person == rank {
                pj += 1;
            }
            while ci < c.len() && c[ci].person < rank {
                ci += 1;
            }
            let mut cj = ci;
            while cj < c.len() && c[cj].person == rank {
                cj += 1;
            }
            self.person_buckets(persons[rank as usize], &aggs[i..j], &purchases[pi..pj], &c[ci..cj]);
            i = j;
            pi = pj;
            ci = cj;
        }
        crate::prof::lap(crate::prof::SESS_PERSON, t);
        self.aggs = aggs;
        self.purchases = purchases;
        self.cls = c;
        self.persons = persons;
    }

    /// `person` from one person's buckets (sorted), purchases and clicks (sorted by ts).
    #[inline(never)]
    fn person_buckets(&mut self, person: u64, buckets: &[Agg], purchases: &[PersonEvent], cls: &[PersonClick]) {
        let first_day = self.shape.first_day;
        let base = self.shape.base;
        let (mut lo, mut hi) = (0usize, 0usize);
        let mut current_day = i64::MIN;
        let (mut day_sessions, mut day_events, mut day_revenue) = (0i64, 0i64, 0i64);
        self.categories.clear();
        let (mut k, mut p) = (0usize, 0usize);
        while k < buckets.len() {
            let start = buckets[k].min as i64;
            let mut last = buckets[k].max;
            let mut count = buckets[k].count as i64;
            let mut end = k + 1;
            while end < buckets.len() && (buckets[end].min - last) as i64 <= GAP {
                last = buckets[end].max;
                count += buckets[end].count as i64;
                end += 1;
            }
            let day = (base + start).div_euclid(DAY);
            if day != current_day {
                if current_day != i64::MIN {
                    self.flush(person, current_day, day_sessions, day_events, day_revenue);
                }
                current_day = day;
                day_sessions = 0;
                day_events = 0;
                day_revenue = 0;
                self.categories.clear();
            }
            // this session's purchases: every purchase up to its last ts
            let mut revenue = 0i64;
            let mut purchased = false;
            while p < purchases.len() && (purchases[p].packed >> 32) as u32 <= last {
                let e = purchases[p];
                let cents = price_of(e.packed) as i64;
                let category = category_of(e.packed);
                revenue += cents;
                purchased = true;
                match self.categories.iter_mut().find(|c| c.0 == category) {
                    Some(c) => c.1 += cents,
                    None => self.categories.push((category, cents)),
                }
                p += 1;
            }
            day_sessions += 1;
            day_events += count;
            day_revenue += revenue;

            while hi < cls.len() && cls[hi].ts as i64 <= start {
                self.enter(cls[hi].campaign);
                hi += 1;
            }
            let low = start - WEEK;
            while lo < hi && (cls[lo].ts as i64) < low {
                self.leave(cls[lo].campaign);
                lo += 1;
            }
            let di = (day - first_day) as usize;
            let credited = if lo < hi {
                let last_click = cls[hi - 1].ts;
                let mut best = hi - 1;
                let mut b = hi - 1;
                while b > lo && cls[b - 1].ts == last_click {
                    b -= 1;
                    if key::cmp(cls[b].click_id, cls[best].click_id) == Ordering::Less {
                        best = b;
                    }
                }
                let row = &mut self.assists[di * (self.shape.campaigns + 1)..(di + 1) * (self.shape.campaigns + 1)];
                for &c in &self.active {
                    row[c as usize + 1] += 1;
                }
                cls[best].campaign as usize + 1
            } else {
                0
            };
            let s = &mut self.credited[di * (self.shape.campaigns + 1) + credited];
            s.0 += 1;
            s.1 += purchased as u32;
            s.2 += revenue;
            k = end;
        }
        if current_day != i64::MIN {
            self.flush(person, current_day, day_sessions, day_events, day_revenue);
        }
        for c in &cls[lo..hi] {
            self.leave(c.campaign);
        }
    }

    /// One person's events (sorted by ts) and valid clicks (sorted by ts).
    #[inline(never)]
    fn person(&mut self, person: u64, evs: &[PersonEvent], cls: &[PersonClick]) {
        let first_day = self.shape.first_day;
        let base = self.shape.base;
        let (mut lo, mut hi) = (0usize, 0usize);
        let mut current_day = i64::MIN;
        let (mut day_sessions, mut day_events, mut day_revenue) = (0i64, 0i64, 0i64);
        self.categories.clear();
        let mut k = 0;
        while k < evs.len() {
            let start = ts_of(evs[k].packed);
            let mut end = k + 1;
            while end < evs.len() && ts_of(evs[end].packed) - ts_of(evs[end - 1].packed) <= GAP {
                end += 1;
            }
            let day = (base + start).div_euclid(DAY);
            if day != current_day {
                if current_day != i64::MIN {
                    self.flush(person, current_day, day_sessions, day_events, day_revenue);
                }
                current_day = day;
                day_sessions = 0;
                day_events = 0;
                day_revenue = 0;
                self.categories.clear();
            }
            let mut revenue = 0i64;
            let mut purchased = false;
            for e in &evs[k..end] {
                if e.packed & PURCHASE != 0 {
                    let cents = price_of(e.packed) as i64;
                    let category = category_of(e.packed);
                    revenue += cents;
                    purchased = true;
                    match self.categories.iter_mut().find(|c| c.0 == category) {
                        Some(c) => c.1 += cents,
                        None => self.categories.push((category, cents)),
                    }
                }
            }
            day_sessions += 1;
            day_events += (end - k) as i64;
            day_revenue += revenue;

            while hi < cls.len() && cls[hi].ts as i64 <= start {
                self.enter(cls[hi].campaign);
                hi += 1;
            }
            let low = start - WEEK;
            while lo < hi && (cls[lo].ts as i64) < low {
                self.leave(cls[lo].campaign);
                lo += 1;
            }
            let di = (day - first_day) as usize;
            let credited = if lo < hi {
                let last = cls[hi - 1].ts;
                let mut best = hi - 1;
                let mut b = hi - 1;
                while b > lo && cls[b - 1].ts == last {
                    b -= 1;
                    if key::cmp(cls[b].click_id, cls[best].click_id) == Ordering::Less {
                        best = b;
                    }
                }
                let row = &mut self.assists[di * (self.shape.campaigns + 1)..(di + 1) * (self.shape.campaigns + 1)];
                for &c in &self.active {
                    row[c as usize + 1] += 1;
                }
                cls[best].campaign as usize + 1
            } else {
                0
            };
            let s = &mut self.credited[di * (self.shape.campaigns + 1) + credited];
            s.0 += 1;
            s.1 += purchased as u32;
            s.2 += revenue;
            k = end;
        }
        if current_day != i64::MIN {
            self.flush(person, current_day, day_sessions, day_events, day_revenue);
        }
        for c in &cls[lo..hi] {
            self.leave(c.campaign);
        }
    }
}

/// Stable LSD radix sort of `v` by the low `bits` bits of `key`, 11 bits a pass
/// (all histograms counted in one read); passes whose digit never varies are
/// skipped. `tmp` is scratch.
fn radix_sort<T: Copy + Default>(v: &mut Vec<T>, tmp: &mut Vec<T>, bits: u32, key: impl Fn(&T) -> u64) {
    const DIGIT: u32 = 11;
    const BUCKETS: usize = 1 << DIGIT;
    let n = v.len();
    if n < 2 {
        return;
    }
    if n <= 64 {
        v.sort_by_key(|x| key(x)); // stable, as the passes are
        return;
    }
    let passes = bits.div_ceil(DIGIT) as usize;
    let mut counts = vec![[0u32; BUCKETS]; passes];
    for x in v.iter() {
        let k = key(x);
        for (p, count) in counts.iter_mut().enumerate() {
            count[((k >> (p as u32 * DIGIT)) as usize) & (BUCKETS - 1)] += 1;
        }
    }
    tmp.clear();
    tmp.resize(n, T::default());
    for (p, count) in counts.iter_mut().enumerate() {
        if count.iter().any(|&c| c as usize == n) {
            continue; // every key has the same digit here
        }
        let mut at = 0u32;
        for c in count.iter_mut() {
            let here = *c;
            *c = at;
            at += here;
        }
        let shift = p as u32 * DIGIT;
        for x in v.iter() {
            let d = ((key(x) >> shift) as usize) & (BUCKETS - 1);
            // SAFETY: the prefix sums keep every position below n.
            unsafe { *tmp.get_unchecked_mut(count[d] as usize) = *x };
            count[d] += 1;
        }
        std::mem::swap(v, tmp);
    }
}

/// Person -> dense rank for one partition: open addressing over (person, rank +
/// generation) slots, never cleared (a slot of an older generation is empty).
#[derive(Default)]
pub struct Ranks {
    slots: Vec<(u64, u32, u32)>,
    generation: u32,
    len: usize,
}

impl Ranks {
    fn reset(&mut self) {
        if self.slots.is_empty() {
            self.slots = vec![(0, 0, 0); 4096];
        }
        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            self.slots.iter_mut().for_each(|s| *s = (0, 0, 0));
            self.generation = 1;
        }
        self.len = 0;
    }

    #[inline]
    fn find(&self, person: u64) -> Option<u32> {
        let mask = self.slots.len() - 1;
        let mut i = mix(person) as usize & mask;
        loop {
            let (k, rank, generation) = self.slots[i];
            if generation != self.generation {
                return None;
            }
            if k == person {
                return Some(rank);
            }
            i = (i + 1) & mask;
        }
    }

    /// The person's rank, a new one (pushed on `persons`) at first sight.
    #[inline]
    fn rank(&mut self, person: u64, persons: &mut Vec<u64>) -> u32 {
        let mask = self.slots.len() - 1;
        let mut i = mix(person) as usize & mask;
        loop {
            let (k, rank, generation) = self.slots[i];
            if generation != self.generation {
                let rank = persons.len() as u32;
                self.slots[i] = (person, rank, self.generation);
                persons.push(person);
                self.len += 1;
                if self.len * 2 > self.slots.len() {
                    self.grow(persons);
                }
                return rank;
            }
            if k == person {
                return rank;
            }
            i = (i + 1) & mask;
        }
    }

    fn grow(&mut self, persons: &[u64]) {
        let size = self.slots.len() * 2;
        self.slots = vec![(0, 0, 0); size];
        self.generation = 1;
        let mask = size - 1;
        for (rank, &person) in persons.iter().enumerate() {
            let mut i = mix(person) as usize & mask;
            while self.slots[i].2 == self.generation {
                i = (i + 1) & mask;
            }
            self.slots[i] = (person, rank as u32, self.generation);
        }
    }
}
