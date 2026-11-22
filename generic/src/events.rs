//! Event and click lines to records.

use crate::arena::Scatter;
use crate::hash::{bucket, mix};
use crate::key;
use crate::links::Names;
use crate::sessions::PersonClick;
use crate::lookups::{Campaigns, Products};
use crate::ts::{self, DAY};

pub const SURVIVES: u8 = 1;
pub const PURCHASE: u8 = 2;
/// `user` already holds the person's name.
pub const RESOLVED: u8 = 4;
pub const WEEK: i64 = 7 * DAY;

/// One well-formed copy of an event with a non-empty event_id. `flags` says whether
/// the copy would survive validation and the late check if it is the kept copy; the
/// other fields only matter when it survives.
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct EventCopy {
    pub event_id: u64,
    pub ingest: i64,
    pub user: u64,
    pub ts: i64,
    pub price: u32,
    pub category: u16,
    pub flags: u8,
}

/// A short FIFO of values whose name lookups were prefetched: a value comes out
/// LAG pushes after it went in, by which time its cache line has arrived.
pub struct Pending<T: Copy + Default> {
    buf: [T; LAG],
    head: usize,
    len: usize,
}

const LAG: usize = 16;

impl<T: Copy + Default> Pending<T> {
    pub fn new() -> Self {
        Self { buf: [T::default(); LAG], head: 0, len: 0 }
    }

    /// Adds `value`; returns the oldest value once LAG are waiting.
    #[inline]
    pub fn push(&mut self, value: T) -> Option<T> {
        if self.len < LAG {
            self.buf[(self.head + self.len) % LAG] = value;
            self.len += 1;
            None
        } else {
            let out = self.buf[self.head];
            self.buf[self.head] = value;
            self.head = (self.head + 1) % LAG;
            Some(out)
        }
    }

    #[inline]
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let out = self.buf[self.head];
        self.head = (self.head + 1) % LAG;
        self.len -= 1;
        Some(out)
    }
}

pub struct Context<'a> {
    pub window_start: i64,
    pub window_end: i64,
    pub products: &'a Products,
    pub campaigns: &'a Campaigns,
}

/// `price_cents` of a purchase: ASCII digits only, 1 to 10,000,000.
#[inline]
fn price(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let first = b.iter().position(|&c| c != b'0')?;
    let digits = &b[first..];
    if digits.len() > 8 {
        return None;
    }
    let value = digits.iter().fold(0u32, |v, &c| v * 10 + (c - b'0') as u32);
    (1..=10_000_000).contains(&value).then_some(value)
}

#[inline]
pub fn event_fields(
    f: &[&[u8]; 7],
    cx: &Context,
    names: Option<&Names>,
    pending: &mut Pending<EventCopy>,
    parts: &mut Scatter<EventCopy>,
) {
    let [event_id, user, ts, ingest, event_type, product, price_cents] = *f;
    if event_id.is_empty() {
        return;
    }
    let mut copy = EventCopy {
        event_id: key::key(event_id),
        ingest: ts::parse(ingest).unwrap_or(i64::MAX),
        user: 0,
        ts: 0,
        price: 0,
        category: 0,
        flags: 0,
    };
    'valid: {
        if user.is_empty() || product.is_empty() {
            break 'valid;
        }
        let purchase = match event_type {
            b"view" | b"cart" => false,
            b"purchase" => true,
            _ => break 'valid,
        };
        // The name lookup misses the cache: start loading it before the rest.
        let user_key = key::key(user);
        if let Some(names) = names {
            names.prefetch(user_key);
        }
        let t = match ts::parse(ts) {
            Some(t) if t >= cx.window_start && t < cx.window_end => t,
            _ => break 'valid,
        };
        if purchase {
            match price(price_cents) {
                Some(p) => copy.price = p,
                None => break 'valid,
            }
            copy.category = cx.products.category(product);
        }
        if copy.ingest.saturating_sub(t) > DAY {
            break 'valid;
        }
        copy.ts = t;
        copy.flags = SURVIVES | if purchase { PURCHASE } else { 0 };
        copy.user = user_key;
        if let Some(names) = names {
            // Resolved LAG lines later, once the prefetched slot has arrived.
            copy.flags |= RESOLVED;
            if let Some(mut old) = pending.push(copy) {
                old.user = names.get(old.user);
                parts.push(bucket(mix(old.event_id), parts.parts()), old);
            }
            return;
        }
    }
    let p = bucket(mix(copy.event_id), parts.parts());
    parts.push(p, copy);
}

/// Resolves and stores what is still waiting in `pending`.
pub fn flush_events(names: &Names, pending: &mut Pending<EventCopy>, parts: &mut Scatter<EventCopy>) {
    while let Some(mut old) = pending.pop() {
        old.user = names.get(old.user);
        parts.push(bucket(mix(old.event_id), parts.parts()), old);
    }
}

#[inline]
pub fn click_fields(
    f: &[&[u8]; 4],
    cx: &Context,
    names: &Names,
    pending: &mut Pending<PersonClick>,
    out: &mut Scatter<PersonClick>,
) {
    let [click_id, user, campaign, ts] = *f;
    if click_id.is_empty() || user.is_empty() {
        return;
    }
    let user_key = key::key(user);
    names.prefetch(user_key);
    let t = match ts::parse(ts) {
        Some(t) if t >= cx.window_start.saturating_sub(WEEK) && t < cx.window_end => t,
        _ => return,
    };
    let Some(campaign) = cx.campaigns.get(campaign) else {
        return;
    };
    let click = PersonClick {
        person: user_key,
        ts: t,
        click_id: key::key(click_id),
        campaign,
    };
    if let Some(mut old) = pending.push(click) {
        old.person = names.get(old.person);
        out.push(bucket(mix(old.person), out.parts()), old);
    }
}

/// Resolves and stores the clicks still waiting in `pending`.
pub fn flush_clicks(names: &Names, pending: &mut Pending<PersonClick>, out: &mut Scatter<PersonClick>) {
    while let Some(mut old) = pending.pop() {
        old.person = names.get(old.person);
        out.push(bucket(mix(old.person), out.parts()), old);
    }
}

#[inline]
pub fn link_fields(f: &[&[u8]; 2], out: &mut Vec<(u64, u64)>) {
    let [a, b] = *f;
    if a.is_empty() || b.is_empty() {
        return;
    }
    out.push((key::key(a), key::key(b)));
}

#[cfg(test)]
mod tests {
    use super::price;

    #[test]
    fn prices() {
        for (s, v) in [("1", Some(1)), ("0250", Some(250)), ("10000000", Some(10_000_000)), ("0000000000010000000", Some(10_000_000))] {
            assert_eq!(price(s.as_bytes()), v, "{s}");
        }
        for s in ["", "0", "00", "10000001", "250000000", "19.99", "-5", "+500", "1e3", " 1", "1\r", "٢٥٠", "２５０"] {
            assert_eq!(price(s.as_bytes()), None, "{s:?}");
        }
    }
}
