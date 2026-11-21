//! Shopping Sessions: a hand-written pipeline for rules.md.
//!
//! Phase 1 (parallel, newline-aligned chunks of mmapped CSVs): parse events and
//! clicks; events are routed by a hash of event_id. Links are joined on their
//! own thread meanwhile (union-find, smallest ID names the person).
//! Phase 2 (parallel over event_id partitions): dedup on event_id keeping the
//! smallest ingest_ts, then route surviving events (and clicks) by person.
//! Phase 3 (parallel over person partitions): sort, sessionize, credit and
//! assist, aggregate. Each worker writes its own user_daily Parquet file.

mod csv;
mod keys;
mod out;
mod ts;

use std::collections::{BTreeSet, HashMap};
use std::hash::{BuildHasherDefault, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::thread;
use std::time::Instant;

use memmap2::Mmap;
use mimalloc::MiMalloc;

use ts::DAY;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const GAP: i64 = 30 * 60 * 1000;
const REACH: i64 = 7 * DAY;
const P_BITS: u32 = 8;
const P: usize = 1 << P_BITS;
const Q_BITS: u32 = 8;
const Q: usize = 1 << Q_BITS;
const INVALID: u32 = u32::MAX;
const NONE: u32 = u32::MAX;
const CHUNK: usize = 4 << 20;
const EMPTY: u64 = u64::MAX;

#[inline(always)]
fn mix(k: u64) -> u64 {
    let mut x = k ^ (k >> 31);
    x = x.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 29;
    x.wrapping_mul(0xBF58_476D_1CE4_E5B9)
}

#[inline(always)]
fn part(k: u64, bits: u32) -> usize {
    (mix(k) >> (64 - bits)) as usize
}

#[derive(Default)]
struct U64Hasher(u64);
impl Hasher for U64Hasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x517C_C1B7_2722_0A95);
        }
    }
    fn write_u64(&mut self, v: u64) {
        self.0 = mix(v);
    }
}
type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<U64Hasher>>;

/// ID -> value, with a dense array for `<prefix><canonical number>` IDs.
struct Lookup {
    prefix: &'static [u8],
    by_num: Vec<u32>,
    map: HashMap<Vec<u8>, u32>,
}

impl Lookup {
    fn new(prefix: &'static [u8]) -> Self {
        Self { prefix, by_num: Vec::new(), map: HashMap::new() }
    }
    #[inline(always)]
    fn number(&self, s: &[u8]) -> Option<usize> {
        let p = self.prefix.len();
        if s.len() <= p || s.len() > p + 7 || &s[..p] != self.prefix || s[p] == b'0' {
            return None;
        }
        let mut v = 0usize;
        for &b in &s[p..] {
            let d = b.wrapping_sub(b'0');
            if d >= 10 {
                return None;
            }
            v = v * 10 + d as usize;
        }
        Some(v)
    }
    fn insert(&mut self, id: &[u8], value: u32) {
        match self.number(id) {
            Some(n) => {
                if self.by_num.len() <= n {
                    self.by_num.resize(n + 1, NONE);
                }
                self.by_num[n] = value;
            }
            None => {
                self.map.insert(id.to_vec(), value);
            }
        }
    }
    #[inline(always)]
    fn get(&self, s: &[u8]) -> Option<u32> {
        match self.number(s) {
            Some(n) => self.by_num.get(n).copied().filter(|&v| v != NONE),
            None => self.map.get(s).copied(),
        }
    }
}

struct Ctx {
    ws: i64,
    we: i64,
    cws: i64,
    day0: i64,
    ndays: usize,
    products: Lookup,
    unknown_cat: u32,
    cat_names: Vec<Vec<u8>>,
    campaigns: Lookup,
    camp_ids: Vec<Vec<u8>>,
    channels: Vec<Vec<u8>>,
}

#[derive(Clone, Copy)]
struct Ev {
    ekey: u64,
    ukey: u64,
    ingest: i64,
    ts: i64,
    /// INVALID if this copy fails validation; 0 for view/cart; else price.
    price: u32,
    cat: u32,
}

#[derive(Clone, Copy)]
struct Cl {
    ukey: u64,
    ckey: u64,
    ts: i64,
    camp: u32,
}

#[derive(Clone, Copy)]
struct PEv {
    person: u64,
    ts: i64,
    price: u32,
    cat: u32,
}

#[derive(Clone, Copy)]
struct PCl {
    person: u64,
    ts: i64,
    ckey: u64,
    camp: u32,
}

fn pool<S: Send>(
    threads: usize,
    tasks: usize,
    init: impl Fn() -> S + Sync,
    work: impl Fn(&mut S, usize) + Sync,
) -> Vec<S> {
    let next = AtomicUsize::new(0);
    thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut state = init();
                    loop {
                        let t = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if t >= tasks {
                            break;
                        }
                        work(&mut state, t);
                    }
                    state
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

fn list_csv(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "csv") && p.is_file())
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

fn map_files(paths: &[PathBuf]) -> Vec<Mmap> {
    paths
        .iter()
        .filter_map(|p| {
            let f = std::fs::File::open(p).expect("open input");
            if f.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
                return None;
            }
            Some(unsafe { Mmap::map(&f).expect("mmap input") })
        })
        .collect()
}

/// The data lines of a file: everything after the header line.
fn body(m: &[u8]) -> &[u8] {
    match memchr::memchr(b'\n', m) {
        Some(i) => &m[i + 1..],
        None => &[],
    }
}

/// Newline-aligned chunks (file index, start, end) of every file's body.
fn chunks(maps: &[Mmap]) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for (fi, m) in maps.iter().enumerate() {
        let len = m.len();
        let mut pos = len - body(m).len();
        while pos < len {
            let mut end = (pos + CHUNK).min(len);
            if end < len {
                end = match memchr::memchr(b'\n', &m[end..]) {
                    Some(i) => end + i + 1,
                    None => len,
                };
            }
            out.push((fi, pos, end));
            pos = end;
        }
    }
    out
}

fn read_small(path: &Path) -> Vec<(Vec<u8>, Vec<u8>)> {
    let data = std::fs::read(path).unwrap_or_default();
    let mut rows = Vec::new();
    csv::for_lines(body(&data), |line| {
        let mut f = [&b""[..]; 2];
        if csv::split(line, &mut f) && !f[0].is_empty() {
            rows.push((f[0].to_vec(), f[1].to_vec()));
        }
    });
    rows
}

fn manifest_value(text: &str, key: &str) -> i64 {
    let at = text.find(&format!("\"{key}\"")).expect("manifest key");
    let rest = &text[at + key.len() + 2..];
    let rest = &rest[rest.find(':').expect("manifest colon") + 1..];
    let rest = &rest[rest.find('"').expect("manifest quote") + 1..];
    let value = &rest[..rest.find('"').expect("manifest quote")];
    ts::parse(value.as_bytes()).expect("manifest timestamp")
}

fn load_ctx(data: &Path) -> Ctx {
    let manifest = std::fs::read_to_string(data.join("manifest.json")).expect("manifest.json");
    let ws = manifest_value(&manifest, "window_start");
    let we = manifest_value(&manifest, "window_end");

    let product_rows = read_small(&data.join("products.csv"));
    let mut names: BTreeSet<Vec<u8>> = product_rows.iter().map(|r| r.1.clone()).collect();
    names.insert(b"unknown".to_vec());
    let cat_names: Vec<Vec<u8>> = names.into_iter().collect();
    let cat_index: HashMap<&[u8], u32> =
        cat_names.iter().enumerate().map(|(i, n)| (n.as_slice(), i as u32)).collect();
    let mut products = Lookup::new(b"p");
    for (id, cat) in &product_rows {
        products.insert(id, cat_index[cat.as_slice()]);
    }
    let unknown_cat = cat_index[&b"unknown"[..]];

    let campaign_rows = read_small(&data.join("campaigns.csv"));
    let mut campaigns = Lookup::new(b"c_");
    let mut camp_ids = Vec::new();
    let mut channels = Vec::new();
    for (id, channel) in campaign_rows {
        campaigns.insert(&id, camp_ids.len() as u32);
        camp_ids.push(id);
        channels.push(channel);
    }
    let day0 = ws.div_euclid(DAY);
    let ndays = ((we - 1).div_euclid(DAY) - day0 + 1).max(1) as usize;
    Ctx {
        ws,
        we,
        cws: ws - REACH,
        day0,
        ndays,
        products,
        unknown_cat,
        cat_names,
        campaigns,
        camp_ids,
        channels,
    }
}

#[inline(always)]
fn parse_price(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &b in s {
        let d = b.wrapping_sub(b'0');
        if d >= 10 {
            return None;
        }
        v = v * 10 + d as u64;
        if v > 10_000_000 {
            return None;
        }
    }
    if v == 0 {
        None
    } else {
        Some(v as u32)
    }
}

#[inline(always)]
fn parse_event(line: &[u8], ctx: &Ctx, buckets: &mut [Vec<Ev>]) {
    let mut f = [&b""[..]; 7];
    if !csv::split(line, &mut f) || f[0].is_empty() {
        return;
    }
    let ekey = keys::key(f[0]);
    let ingest = ts::parse(f[3]).unwrap_or(i64::MAX);
    let mut ev = Ev { ekey, ukey: 0, ingest, ts: 0, price: INVALID, cat: 0 };
    'valid: {
        if f[1].is_empty() || f[5].is_empty() {
            break 'valid;
        }
        let Some(t) = ts::parse(f[2]) else { break 'valid };
        if t < ctx.ws || t >= ctx.we {
            break 'valid;
        }
        let price = match f[4] {
            b"view" | b"cart" => 0,
            b"purchase" => match parse_price(f[6]) {
                Some(p) => p,
                None => break 'valid,
            },
            _ => break 'valid,
        };
        if ingest.saturating_sub(t) > DAY {
            break 'valid;
        }
        ev.ukey = keys::key(f[1]);
        ev.ts = t;
        ev.price = price;
        if price > 0 {
            ev.cat = ctx.products.get(f[5]).unwrap_or(ctx.unknown_cat);
        }
    }
    buckets[part(ekey, P_BITS)].push(ev);
}

#[inline(always)]
fn parse_click(line: &[u8], ctx: &Ctx, out: &mut Vec<Cl>) {
    let mut f = [&b""[..]; 4];
    if !csv::split(line, &mut f) || f[0].is_empty() || f[1].is_empty() {
        return;
    }
    let Some(t) = ts::parse(f[3]) else { return };
    if t < ctx.cws || t >= ctx.we {
        return;
    }
    let Some(camp) = ctx.campaigns.get(f[2]) else { return };
    out.push(Cl { ukey: keys::key(f[1]), ckey: keys::key(f[0]), ts: t, camp });
}

/// Open-addressing u64 -> u64 map, built once, read by all workers.
struct FlatMap {
    keys: Vec<u64>,
    vals: Vec<u64>,
    mask: usize,
}

impl FlatMap {
    fn from_pairs(pairs: &[(u64, u64)]) -> Self {
        let cap = (pairs.len() * 2).next_power_of_two().max(16);
        let mut m = FlatMap { keys: vec![EMPTY; cap], vals: vec![0; cap], mask: cap - 1 };
        for &(k, v) in pairs {
            let mut h = mix(k) as usize & m.mask;
            while m.keys[h] != EMPTY && m.keys[h] != k {
                h = (h + 1) & m.mask;
            }
            m.keys[h] = k;
            m.vals[h] = v;
        }
        m
    }
    #[inline(always)]
    fn prefetch(&self, k: u64) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
            let h = mix(k) as usize & self.mask;
            _mm_prefetch(self.keys.as_ptr().add(h) as *const i8, _MM_HINT_T0);
        }
    }
    #[inline(always)]
    fn get_or(&self, k: u64) -> u64 {
        let mut h = mix(k) as usize & self.mask;
        loop {
            let x = self.keys[h];
            if x == k {
                return self.vals[h];
            }
            if x == EMPTY {
                return k;
            }
            h = (h + 1) & self.mask;
        }
    }
}

/// Union-find over the links: every linked ID that is not its person's name,
/// mapped to the name (the byte-wise smallest ID of its component).
fn build_aliases(maps: &[Mmap]) -> FlatMap {
    let mut index: FastMap<u64, u32> = FastMap::default();
    let mut ids: Vec<u64> = Vec::new();
    let mut parent: Vec<u32> = Vec::new();
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            let gp = parent[parent[x as usize] as usize];
            parent[x as usize] = gp;
            x = gp;
        }
        x
    }
    let mut id_of = |k: u64, ids: &mut Vec<u64>, parent: &mut Vec<u32>| -> u32 {
        *index.entry(k).or_insert_with(|| {
            ids.push(k);
            parent.push(parent.len() as u32);
            (ids.len() - 1) as u32
        })
    };
    for m in maps {
        csv::for_lines(body(m), |line| {
            let mut f = [&b""[..]; 2];
            if !csv::split(line, &mut f) || f[0].is_empty() || f[1].is_empty() {
                return;
            }
            let a = id_of(keys::key(f[0]), &mut ids, &mut parent);
            let b = id_of(keys::key(f[1]), &mut ids, &mut parent);
            let ra = find(&mut parent, a);
            let rb = find(&mut parent, b);
            if ra != rb {
                parent[ra as usize] = rb;
            }
        });
    }
    let n = ids.len();
    let mut best = vec![EMPTY; n];
    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        let k = ids[i];
        if best[r] == EMPTY || keys::cmp(k, best[r]).is_lt() {
            best[r] = k;
        }
    }
    let mut pairs = Vec::with_capacity(n);
    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        if best[r] != ids[i] {
            pairs.push((ids[i], best[r]));
        }
    }
    FlatMap::from_pairs(&pairs)
}

struct P1 {
    ev: Vec<Vec<Ev>>,
    cl: Vec<Cl>,
}

struct P2 {
    pe: Vec<Vec<PEv>>,
    pc: Vec<Vec<PCl>>,
    buf: Vec<Ev>,
    tkeys: Vec<u64>,
    tvals: Vec<u32>,
    win: Vec<bool>,
    prof: [f64; 4],
}

#[derive(Default)]
struct UserRows {
    names: Vec<u8>,
    name_ends: Vec<u32>,
    days: Vec<i32>,
    sessions: Vec<i64>,
    events: Vec<i64>,
    revenue: Vec<i64>,
    top: Vec<u8>,
    top_ends: Vec<u32>,
    top_defs: Vec<i16>,
}

struct P3 {
    evs: Vec<PEv>,
    cls: Vec<PCl>,
    catsum: Vec<u64>,
    touched: Vec<u32>,
    ccount: Vec<u32>,
    cpos: Vec<u32>,
    active: Vec<u32>,
    /// (campaigns + organic) x days x [sessions, purchasing, revenue, assisted]
    acc: Vec<[i64; 4]>,
    rows: UserRows,
}

impl P3 {
    #[inline(always)]
    fn add(&mut self, camp: u32) {
        let c = camp as usize;
        if self.ccount[c] == 0 {
            self.cpos[c] = self.active.len() as u32;
            self.active.push(camp);
        }
        self.ccount[c] += 1;
    }
    #[inline(always)]
    fn remove(&mut self, camp: u32) {
        let c = camp as usize;
        self.ccount[c] -= 1;
        if self.ccount[c] == 0 {
            let at = self.cpos[c] as usize;
            let last = self.active.pop().unwrap();
            if at < self.active.len() {
                self.active[at] = last;
                self.cpos[last as usize] = at as u32;
            }
        }
    }

    fn flush_day(&mut self, ctx: &Ctx, person: u64, day: i64, s: i64, e: i64, rev: i64) {
        let r = &mut self.rows;
        keys::bytes(person, &mut r.names);
        r.name_ends.push(r.names.len() as u32);
        r.days.push(day as i32);
        r.sessions.push(s);
        r.events.push(e);
        r.revenue.push(rev);
        if self.touched.is_empty() {
            r.top_defs.push(0);
        } else {
            let mut best = self.touched[0];
            for &c in &self.touched[1..] {
                let (sc, sb) = (self.catsum[c as usize], self.catsum[best as usize]);
                if sc > sb || (sc == sb && c < best) {
                    best = c;
                }
            }
            for &c in &self.touched {
                self.catsum[c as usize] = 0;
            }
            self.touched.clear();
            r.top.extend_from_slice(&ctx.cat_names[best as usize]);
            r.top_ends.push(r.top.len() as u32);
            r.top_defs.push(1);
        }
    }

    /// One person's events (sorted by ts) and clicks (sorted by ts, click_id).
    fn person(&mut self, ctx: &Ctx, person: u64, e_lo: usize, e_hi: usize, c_lo: usize, c_hi: usize) {
        let organic = ctx.camp_ids.len();
        let nd = ctx.ndays;
        let (mut lo, mut hi) = (c_lo, c_lo);
        let mut cur_day = i64::MIN;
        let (mut ds, mut de, mut dr) = (0i64, 0i64, 0i64);
        let mut k = e_lo;
        while k < e_hi {
            let start = self.evs[k].ts;
            let day = start.div_euclid(DAY);
            if day != cur_day {
                if cur_day != i64::MIN {
                    self.flush_day(ctx, person, cur_day, ds, de, dr);
                }
                cur_day = day;
                (ds, de, dr) = (0, 0, 0);
            }
            let mut last = start;
            let mut n = 0i64;
            let mut rev = 0i64;
            while k < e_hi {
                let ev = self.evs[k];
                if ev.ts - last > GAP {
                    break;
                }
                last = ev.ts;
                n += 1;
                if ev.price > 0 {
                    rev += ev.price as i64;
                    let c = ev.cat as usize;
                    if self.catsum[c] == 0 {
                        self.touched.push(ev.cat);
                    }
                    self.catsum[c] += ev.price as u64;
                }
                k += 1;
            }
            ds += 1;
            de += n;
            dr += rev;

            while hi < c_hi && self.cls[hi].ts <= start {
                let camp = self.cls[hi].camp;
                self.add(camp);
                hi += 1;
            }
            while lo < hi && self.cls[lo].ts < start - REACH {
                let camp = self.cls[lo].camp;
                self.remove(camp);
                lo += 1;
            }
            let di = (day - ctx.day0) as usize;
            let credited = if hi > lo {
                let t = self.cls[hi - 1].ts;
                let mut m = hi - 1;
                while m > lo && self.cls[m - 1].ts == t {
                    m -= 1;
                }
                self.cls[m].camp as usize
            } else {
                organic
            };
            let a = &mut self.acc[credited * nd + di];
            a[0] += 1;
            a[1] += (rev > 0) as i64;
            a[2] += rev;
            for &camp in &self.active {
                self.acc[camp as usize * nd + di][3] += 1;
            }
        }
        if cur_day != i64::MIN {
            self.flush_day(ctx, person, cur_day, ds, de, dr);
        }
        for &camp in &self.active {
            self.ccount[camp as usize] = 0;
        }
        self.active.clear();
    }
}

fn main() {
    let t0 = Instant::now();
    let profile = std::env::var_os("SHOP_PROFILE").is_some();
    let lap = |what: &str| {
        if profile {
            eprintln!("{:>8.3}s {what}", t0.elapsed().as_secs_f64());
        }
    };
    let data = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into()));
    let outdir = PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into()));
    let threads: usize = std::env::var("NCPU")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| thread::available_parallelism().map_or(8, usize::from));

    let ctx = load_ctx(&data);
    let event_maps = map_files(&list_csv(&data.join("events")));
    let click_maps = map_files(&list_csv(&data.join("clicks")));
    let link_maps = map_files(&list_csv(&data.join("links")));
    let event_chunks = chunks(&event_maps);
    let click_chunks = chunks(&click_maps);
    let n_event_chunks = event_chunks.len();
    let event_bytes: usize = event_maps.iter().map(|m| m.len()).sum();
    let click_bytes: usize = click_maps.iter().map(|m| m.len()).sum();
    // Rows are ~78 (events) and ~47 (clicks) bytes; reserve with headroom.
    let est_events = event_bytes / 70 + 1024;
    let est_clicks = click_bytes / 40 + 1024;
    lap("setup");

    // Phase 1: parse.
    let (p1, aliases) = thread::scope(|scope| {
        let links = scope.spawn(|| build_aliases(&link_maps));
        let p1 = pool(
            threads,
            n_event_chunks + click_chunks.len(),
            || P1 {
                ev: (0..P).map(|_| Vec::with_capacity(est_events / threads / P * 5 / 4)).collect(),
                cl: Vec::with_capacity(est_clicks / threads * 5 / 4),
            },
            |st, t| {
                if t < n_event_chunks {
                    let (fi, a, b) = event_chunks[t];
                    let slice = &event_maps[fi][a..b];
                    csv::for_lines(slice, |line| parse_event(line, &ctx, &mut st.ev));
                } else {
                    let (fi, a, b) = click_chunks[t - n_event_chunks];
                    let slice = &click_maps[fi][a..b];
                    csv::for_lines(slice, |line| parse_click(line, &ctx, &mut st.cl));
                }
            },
        );
        let aliases = links.join().unwrap();
        (p1, aliases)
    });
    lap("parse");

    // Phase 2: dedup events, route events and clicks by person.
    let nw = p1.len();
    let p2 = pool(
        threads,
        P + nw,
        || P2 {
            pe: (0..Q).map(|_| Vec::with_capacity(est_events / threads / Q * 5 / 4)).collect(),
            pc: (0..Q).map(|_| Vec::with_capacity(est_clicks / threads / Q * 5 / 4)).collect(),
            buf: Vec::new(),
            tkeys: Vec::new(),
            tvals: Vec::new(),
            win: Vec::new(),
            prof: [0.0; 4],
        },
        |st, t| {
            let person_of = |u: u64| aliases.get_or(u);
            if t >= P {
                for c in &p1[t - P].cl {
                    let person = person_of(c.ukey);
                    st.pc[part(person, Q_BITS)].push(PCl { person, ts: c.ts, ckey: c.ckey, camp: c.camp });
                }
                return;
            }
            let tt = Instant::now();
            st.buf.clear();
            for w in &p1 {
                st.buf.extend_from_slice(&w.ev[t]);
            }
            let n = st.buf.len();
            if n == 0 {
                return;
            }
            st.prof[0] += tt.elapsed().as_secs_f64();
            let tt = Instant::now();
            let cap = (n * 2).next_power_of_two().max(16);
            let mask = cap - 1;
            st.tkeys.clear();
            st.tkeys.resize(cap, EMPTY);
            st.tvals.resize(cap, 0);
            st.win.clear();
            st.win.resize(n, true);
            for (i, e) in st.buf.iter().enumerate() {
                let mut h = mix(e.ekey) as usize & mask;
                loop {
                    let k = st.tkeys[h];
                    if k == EMPTY {
                        st.tkeys[h] = e.ekey;
                        st.tvals[h] = i as u32;
                        break;
                    }
                    if k == e.ekey {
                        let j = st.tvals[h] as usize;
                        if e.ingest < st.buf[j].ingest {
                            st.tvals[h] = i as u32;
                            st.win[j] = false;
                        } else {
                            st.win[i] = false;
                        }
                        break;
                    }
                    h = (h + 1) & mask;
                }
            }
            st.prof[1] += tt.elapsed().as_secs_f64();
            let tt = Instant::now();
            const AHEAD: usize = 16;
            for i in 0..n.min(AHEAD) {
                aliases.prefetch(st.buf[i].ukey);
            }
            for i in 0..n {
                if i + AHEAD < n {
                    aliases.prefetch(st.buf[i + AHEAD].ukey);
                }
                let e = st.buf[i];
                if !st.win[i] || e.price == INVALID {
                    continue;
                }
                let person = person_of(e.ukey);
                st.pe[part(person, Q_BITS)].push(PEv { person, ts: e.ts, price: e.price, cat: e.cat });
            }
            st.prof[2] += tt.elapsed().as_secs_f64();
        },
    );
    if profile {
        let mut tot = [0.0; 4];
        for w in &p2 { for i in 0..4 { tot[i] += w.prof[i]; } }
        eprintln!("dedup cpu: concat {:.3} table {:.3} emit {:.3}", tot[0], tot[1], tot[2]);
    }
    drop(p1);
    lap("dedup");

    // Phase 3: sessions, attribution, aggregation per person partition.
    let ncamp = ctx.camp_ids.len();
    let p3 = pool(
        threads,
        Q,
        || P3 {
            evs: Vec::new(),
            cls: Vec::new(),
            catsum: vec![0; ctx.cat_names.len()],
            touched: Vec::new(),
            ccount: vec![0; ncamp],
            cpos: vec![0; ncamp],
            active: Vec::new(),
            acc: vec![[0; 4]; (ncamp + 1) * ctx.ndays],
            rows: UserRows::default(),
        },
        |st, q| {
            st.evs.clear();
            st.cls.clear();
            for w in &p2 {
                st.evs.extend_from_slice(&w.pe[q]);
                st.cls.extend_from_slice(&w.pc[q]);
            }
            st.evs.sort_unstable_by(|a, b| (a.person, a.ts).cmp(&(b.person, b.ts)));
            st.cls.sort_unstable_by(|a, b| {
                (a.person, a.ts).cmp(&(b.person, b.ts)).then_with(|| keys::cmp(a.ckey, b.ckey))
            });
            let (ne, nc) = (st.evs.len(), st.cls.len());
            let (mut i, mut ci) = (0, 0);
            while i < ne {
                let person = st.evs[i].person;
                let mut j = i + 1;
                while j < ne && st.evs[j].person == person {
                    j += 1;
                }
                while ci < nc && st.cls[ci].person < person {
                    ci += 1;
                }
                let mut cj = ci;
                while cj < nc && st.cls[cj].person == person {
                    cj += 1;
                }
                st.person(&ctx, person, i, j, ci, cj);
                i = j;
                ci = cj;
            }
        },
    );
    drop(p2);
    lap("sessions");

    // Output.
    let user_dir = outdir.join("user_daily");
    let camp_dir = outdir.join("campaign_daily");
    std::fs::create_dir_all(&user_dir).expect("create user_daily");
    std::fs::create_dir_all(&camp_dir).expect("create campaign_daily");

    let nd = ctx.ndays;
    let mut acc = vec![[0i64; 4]; (ncamp + 1) * nd];
    for st in &p3 {
        for (a, b) in acc.iter_mut().zip(&st.acc) {
            for x in 0..4 {
                a[x] += b[x];
            }
        }
    }
    let mut rows_by_worker: Vec<UserRows> = p3.into_iter().map(|s| s.rows).collect();
    if rows_by_worker.iter().all(|r| r.days.is_empty()) {
        rows_by_worker.truncate(1);
    } else {
        rows_by_worker.retain(|r| !r.days.is_empty());
    }
    thread::scope(|scope| {
        for (w, r) in rows_by_worker.into_iter().enumerate() {
            let path = user_dir.join(format!("part-{w:03}.parquet"));
            scope.spawn(move || {
                out::write(
                    &path,
                    out::USER_SCHEMA,
                    vec![
                        out::Col::Str(r.names, r.name_ends),
                        out::Col::I32(r.days),
                        out::Col::I64(r.sessions),
                        out::Col::I64(r.events),
                        out::Col::I64(r.revenue),
                        out::Col::OptStr(r.top, r.top_ends, r.top_defs),
                    ],
                )
                .expect("write user_daily");
            });
        }
        let (mut ids, mut id_ends, mut chans, mut chan_ends) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut days, mut s, mut ps, mut rev, mut asst) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for c in 0..=ncamp {
            for d in 0..nd {
                let a = acc[c * nd + d];
                if a[0] == 0 && a[3] == 0 {
                    continue;
                }
                let (id, ch): (&[u8], &[u8]) = if c == ncamp {
                    (b"organic", b"organic")
                } else {
                    (&ctx.camp_ids[c], &ctx.channels[c])
                };
                ids.extend_from_slice(id);
                id_ends.push(ids.len() as u32);
                chans.extend_from_slice(ch);
                chan_ends.push(chans.len() as u32);
                days.push((ctx.day0 + d as i64) as i32);
                s.push(a[0]);
                ps.push(a[1]);
                rev.push(a[2]);
                asst.push(a[3]);
            }
        }
        out::write(
            &camp_dir.join("part-000.parquet"),
            out::CAMPAIGN_SCHEMA,
            vec![
                out::Col::Str(ids, id_ends),
                out::Col::Str(chans, chan_ends),
                out::Col::I32(days),
                out::Col::I64(s),
                out::Col::I64(ps),
                out::Col::I64(rev),
                out::Col::I64(asst),
            ],
        )
        .expect("write campaign_daily");
    });
    lap("write");
    // Skip freeing everything: the OS reclaims it at exit.
    std::process::exit(0);
}
