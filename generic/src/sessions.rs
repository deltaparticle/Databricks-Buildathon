//! Dedup of event copies, then per person: sessions, credit, assists and the daily
//! aggregates.

use std::cmp::Ordering;

use crate::arena::Scatter;
use crate::events::{EventCopy, PURCHASE, RESOLVED, SURVIVES, WEEK};
use crate::hash::{bucket, mix};
use crate::key;
use crate::links::Names;
use crate::ts::DAY;

pub const GAP: i64 = 1_800_000;
pub const NO_CATEGORY: u16 = u16::MAX;

#[derive(Clone, Copy)]
#[repr(C)]
pub struct PersonEvent {
    pub person: u64,
    pub ts: i64,
    pub price: u32,
    pub category: u16,
    pub flags: u8,
}

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct PersonClick {
    pub person: u64,
    pub ts: i64,
    pub click_id: u64,
    pub campaign: u32,
}

#[derive(Clone, Copy, Default)]
pub struct Slot {
    event_id: u64,
    /// Position of the kept copy among the partition's copies, in slice order.
    at: u32,
}

/// Keeps, per event_id, the copy with the smallest ingest_ts (copies tied on it are
/// identical) and sends the kept copies that survive to their person's partition.
/// `parts` holds this event_id partition's copies, one slice per parsing thread.
#[inline(never)]
pub fn dedup(
    parts: &[&[EventCopy]],
    names: &Names,
    table: &mut Vec<Slot>,
    kept: &mut Vec<PersonEvent>,
    out: &mut Scatter<PersonEvent>,
) {
    let n: usize = parts.iter().map(|p| p.len()).sum();
    if n == 0 {
        return;
    }
    assert!(n < u32::MAX as usize, "partition too large");
    // A position packs the slice index in its high bits and the offset in the rest.
    let slice_bits = usize::BITS - (parts.len().max(2) - 1).leading_zeros();
    let shift = 32 - slice_bits;
    assert!(parts.iter().all(|p| (p.len() as u64) < (1u64 << shift)), "partition too large");
    let offset_mask = (1u64 << shift) - 1;
    let copy_at = |at: u32| -> &EventCopy {
        let at = at as u64;
        &parts[(at >> shift) as usize][(at & offset_mask) as usize]
    };
    let slots = (n * 2).next_power_of_two();
    let mask = slots - 1;
    table.clear();
    table.resize(slots, Slot::default());
    for (k, part) in parts.iter().enumerate() {
        for (o, c) in part.iter().enumerate() {
            let at = (((k as u64) << shift) | o as u64) as u32;
            let mut i = mix(c.event_id) as usize & mask;
            loop {
                let s = &mut table[i];
                if s.event_id == 0 {
                    *s = Slot { event_id: c.event_id, at };
                    break;
                }
                if s.event_id == c.event_id {
                    // A duplicate (rare): the smaller ingest_ts wins; ties are identical copies.
                    if c.ingest < copy_at(s.at).ingest {
                        s.at = at;
                    }
                    break;
                }
                i = (i + 1) & mask;
            }
        }
    }
    // The kept copies that survive, then their names: the name lookups miss the
    // cache, so each one is prefetched LAG entries ahead.
    kept.clear();
    for s in table.iter() {
        if s.event_id == 0 {
            continue;
        }
        let c = copy_at(s.at);
        if c.flags & SURVIVES != 0 {
            let e = PersonEvent {
                person: c.user,
                ts: c.ts,
                price: c.price,
                category: c.category,
                flags: c.flags,
            };
            if c.flags & RESOLVED != 0 {
                out.push(bucket(mix(e.person), out.parts()), e);
            } else {
                kept.push(e);
            }
        }
    }
    for e in kept.iter().take(LAG) {
        names.prefetch(e.person);
    }
    for i in 0..kept.len() {
        if let Some(ahead) = kept.get(i + LAG) {
            names.prefetch(ahead.person);
        }
        let mut e = kept[i];
        e.person = names.get(e.person);
        out.push(bucket(mix(e.person), out.parts()), e);
    }
}

const LAG: usize = 16;

#[derive(Clone, Copy)]
pub struct Shape {
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
    /// `[(campaign + 1) * days + day]`: credited sessions, purchasing sessions,
    /// revenue, assisted sessions. Index 0 is `organic`.
    pub stats: Vec<[i64; 4]>,
    shape: Shape,
    count: Vec<u32>,
    pos: Vec<u32>,
    active: Vec<u32>,
    categories: Vec<(u16, i64)>,
    evs: Vec<PersonEvent>,
    cls: Vec<PersonClick>,
}

impl Acc {
    pub fn new(shape: Shape) -> Self {
        Self {
            user: Vec::new(),
            day: Vec::new(),
            sessions: Vec::new(),
            events: Vec::new(),
            revenue: Vec::new(),
            top: Vec::new(),
            stats: vec![[0; 4]; (shape.campaigns + 1) * shape.days],
            shape,
            count: vec![0; shape.campaigns],
            pos: vec![u32::MAX; shape.campaigns],
            active: Vec::new(),
            categories: Vec::new(),
            evs: Vec::new(),
            cls: Vec::new(),
        }
    }

    /// Processes one person partition, given as one slice per thread.
    #[inline(never)]
    pub fn partition<'a>(
        &mut self,
        evs: impl Iterator<Item = &'a [PersonEvent]>,
        cls: impl Iterator<Item = &'a [PersonClick]>,
    ) {
        let mut e = std::mem::take(&mut self.evs);
        let mut c = std::mem::take(&mut self.cls);
        e.clear();
        c.clear();
        for part in evs {
            e.extend_from_slice(part);
        }
        for part in cls {
            c.extend_from_slice(part);
        }
        e.sort_unstable_by_key(|x| (x.person, x.ts));
        c.sort_unstable_by_key(|x| (x.person, x.ts));
        let (mut i, mut ci) = (0usize, 0usize);
        while i < e.len() {
            let person = e[i].person;
            let mut j = i + 1;
            while j < e.len() && e[j].person == person {
                j += 1;
            }
            while ci < c.len() && c[ci].person < person {
                ci += 1;
            }
            let mut cj = ci;
            while cj < c.len() && c[cj].person == person {
                cj += 1;
            }
            self.person(person, &e[i..j], &c[ci..cj]);
            i = j;
            ci = cj;
        }
        self.evs = e;
        self.cls = c;
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

    /// One person's events (sorted by ts) and valid clicks (sorted by ts).
    #[inline(never)]
    fn person(&mut self, person: u64, evs: &[PersonEvent], cls: &[PersonClick]) {
        let days = self.shape.days;
        let first_day = self.shape.first_day;
        let (mut lo, mut hi) = (0usize, 0usize);
        let mut current_day = i64::MIN;
        let (mut day_sessions, mut day_events, mut day_revenue) = (0i64, 0i64, 0i64);
        self.categories.clear();
        let mut k = 0;
        while k < evs.len() {
            let start = evs[k].ts;
            let mut end = k + 1;
            while end < evs.len() && evs[end].ts - evs[end - 1].ts <= GAP {
                end += 1;
            }
            let day = start.div_euclid(DAY);
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
                if e.flags & PURCHASE != 0 {
                    let cents = e.price as i64;
                    revenue += cents;
                    purchased = true;
                    match self.categories.iter_mut().find(|c| c.0 == e.category) {
                        Some(c) => c.1 += cents,
                        None => self.categories.push((e.category, cents)),
                    }
                }
            }
            day_sessions += 1;
            day_events += (end - k) as i64;
            day_revenue += revenue;

            while hi < cls.len() && cls[hi].ts <= start {
                self.enter(cls[hi].campaign);
                hi += 1;
            }
            let low = start - WEEK;
            while lo < hi && cls[lo].ts < low {
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
                for &c in &self.active {
                    self.stats[(c as usize + 1) * days + di][3] += 1;
                }
                cls[best].campaign as usize + 1
            } else {
                0
            };
            let s = &mut self.stats[credited * days + di];
            s[0] += 1;
            s[1] += purchased as i64;
            s[2] += revenue;
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
