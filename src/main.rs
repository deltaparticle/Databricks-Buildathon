//! Shopping Sessions: a hand-written pipeline for rules.md.
//!
//! Phase 1 (parallel, newline-aligned chunks of mmapped CSVs): parse events and
//! clicks; events are routed by a hash of event_id. Links are joined on their
//! own thread meanwhile (union-find, smallest ID names the person).
//! Phase 2 (parallel over event_id partitions): dedup on event_id keeping the
//! smallest ingest_ts (a bitmap sends only possible duplicates through a hash
//! table), then route surviving events and clicks by person.
//! Phase 3 (parallel over person partitions): group by person with a counting
//! sort, sort each person's rows by time, sessionize, credit and assist,
//! aggregate. Each worker writes its own user_daily Parquet file.

mod csv;
mod keys;
mod out;
mod ts;

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::thread;
use std::time::Instant;

use mimalloc::MiMalloc;

use ts::DAY;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const GAP: i64 = 30 * 60 * 1000;
const REACH: i64 = 7 * DAY;
const P_BITS: u32 = 9;
const P: usize = 1 << P_BITS;
const Q_BITS: u32 = 6;
const Q: usize = 1 << Q_BITS;
const NONE: u32 = u32::MAX;
const CLICK_SLICES: usize = 4;
const CHUNK: usize = 2 << 20;
const EMPTY: u64 = u64::MAX;
/// `ip` = ingest seconds (biased, 40 bits) << 24 | price (24 bits).
const PRICE_MASK: u64 = (1 << 24) - 1;
const PRICE_INVALID: u64 = PRICE_MASK;
const INGEST_BIAS: i64 = 1 << 39;

#[inline(always)]
fn mix(k: u64) -> u64 {
    let mut x = k ^ (k >> 31);
    x = x.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 29;
    x.wrapping_mul(0xBF58_476D_1CE4_E5B9)
}

#[inline(always)]
fn part(h: u64, bits: u32) -> usize {
    (h >> (64 - bits)) as usize
}

#[inline(always)]
fn prefetch<T>(p: *const T) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::{_mm_prefetch, _MM_HINT_T0};
        _mm_prefetch(p as *const i8, _MM_HINT_T0);
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = p;
}

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

/// A parsed event copy, routed by event_id.
#[derive(Clone, Copy)]
#[repr(C)]
struct Ev {
    ekey: u64,
    ukey: u64,
    /// ingest << 24 | price; price PRICE_INVALID if the copy fails validation.
    ip: u64,
    /// ts - window_start.
    ts: u32,
    cat: u32,
}

/// A valid click.
#[derive(Clone, Copy)]
struct Cl {
    ukey: u64,
    ckey: u64,
    /// ts - (window_start - 7 days).
    ts: u32,
    camp: u32,
}

/// A surviving event, routed by person.
#[derive(Clone, Copy)]
struct PEv {
    person: u64,
    /// ts << 32 | price << 8 | category: sorts by time.
    w: u64,
}

/// A click, routed by person.
#[derive(Clone, Copy)]
struct PCl {
    person: u64,
    ckey: u64,
    ts: u32,
    camp: u32,
}


/// One click of a grouped person.
#[derive(Clone, Copy, Default)]
struct C3 {
    ckey: u64,
    ts: u32,
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

/// An input CSV read with positioned reads (no mmap: no page-fault storm).
struct InFile {
    file: std::fs::File,
    len: usize,
    /// Offset of the first data line (just after the header line).
    body: usize,
}

#[inline]
fn read_at(file: &std::fs::File, buf: &mut [u8], mut off: u64) -> usize {
    let mut done = 0;
    while done < buf.len() {
        #[cfg(unix)]
        let r = std::os::unix::fs::FileExt::read_at(file, &mut buf[done..], off);
        #[cfg(windows)]
        let r = std::os::windows::fs::FileExt::seek_read(file, &mut buf[done..], off);
        match r {
            Ok(0) => break,
            Ok(n) => {
                done += n;
                off += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => panic!("read input: {e}"),
        }
    }
    done
}

fn open_files(paths: &[PathBuf]) -> Vec<InFile> {
    let mut out = Vec::new();
    for p in paths {
        let file = std::fs::File::open(p).expect("open input");
        let len = file.metadata().map(|m| m.len() as usize).unwrap_or(0);
        if len == 0 {
            continue;
        }
        let mut body = len;
        let mut buf = vec![0u8; 4096];
        let mut off = 0;
        while off < len {
            let n = read_at(&file, &mut buf, off as u64);
            if n == 0 {
                break;
            }
            if let Some(i) = memchr::memchr(b'\n', &buf[..n]) {
                body = off + i + 1;
                break;
            }
            off += n;
        }
        out.push(InFile { file, len, body });
    }
    out
}

/// Start the kernel reading every file now (cold page cache).
#[cfg(unix)]
fn fadvise_all(files: &[&InFile]) {
    use std::os::unix::io::AsRawFd;
    for f in files {
        unsafe {
            libc::posix_fadvise(f.file.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL);
            libc::posix_fadvise(f.file.as_raw_fd(), 0, 0, libc::POSIX_FADV_WILLNEED);
        }
    }
}

/// Byte ranges (file index, start, end) of every body; a range owns the lines
/// that start inside it.
fn chunks(files: &[InFile]) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    for (fi, f) in files.iter().enumerate() {
        let mut pos = f.body;
        while pos < f.len {
            let end = (pos + CHUNK).min(f.len);
            out.push((fi, pos, end));
            pos = end;
        }
    }
    out
}

/// Read the lines starting in [start, end) into `buf`; returns them as a slice.
fn load_chunk<'a>(f: &InFile, start: usize, end: usize, buf: &'a mut Vec<u8>) -> &'a [u8] {
    let from = if start > f.body { start - 1 } else { start };
    let n = end - from;
    if buf.len() < n + 4096 {
        buf.resize(n + 4096, 0);
    }
    let got = read_at(&f.file, &mut buf[..n], from as u64);
    let first = if from == start {
        0
    } else {
        match memchr::memchr(b'\n', &buf[..got]) {
            Some(i) => i + 1,
            None => return &[],
        }
    };
    let mut stop = got;
    if got > 0 && buf[got - 1] != b'\n' && end < f.len {
        // Finish the last line from the following bytes.
        let mut off = end;
        loop {
            if buf.len() < stop + 4096 {
                buf.resize(stop + 4096, 0);
            }
            let m = read_at(&f.file, &mut buf[stop..stop + 4096], off as u64);
            if m == 0 {
                break;
            }
            if let Some(i) = memchr::memchr(b'\n', &buf[stop..stop + m]) {
                stop += i + 1;
                break;
            }
            stop += m;
            off += m;
        }
    }
    if first >= stop {
        return &[];
    }
    &buf[first..stop]
}

/// The data lines of a file: everything after the header line.
fn body(m: &[u8]) -> &[u8] {
    match memchr::memchr(b'\n', m) {
        Some(i) => &m[i + 1..],
        None => &[],
    }
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
    assert!(
        we - ws + REACH < u32::MAX as i64,
        "window longer than ~42 days is not supported by the compact records"
    );

    let product_rows = read_small(&data.join("products.csv"));
    let mut names: BTreeSet<Vec<u8>> = product_rows.iter().map(|r| r.1.clone()).collect();
    names.insert(b"unknown".to_vec());
    let cat_names: Vec<Vec<u8>> = names.into_iter().collect();
    assert!(cat_names.len() <= 256, "more than 255 product categories is not supported by the compact records");
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
fn parse_event(f: &[&[u8]; 7], ctx: &Ctx, buckets: &mut [Vec<Ev>]) {
    if f[0].is_empty() {
        return;
    }
    let ekey = keys::key(f[0]);
    let ingest = ts::parse(f[3]);
    let ingest_key = match ingest {
        Some(t) => (t.div_euclid(1000) + INGEST_BIAS).clamp(0, (1 << 40) - 1) as u64,
        None => (1 << 40) - 1,
    };
    let mut ev = Ev { ekey, ukey: 0, ip: ingest_key << 24 | PRICE_INVALID, ts: 0, cat: 0 };
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
        match ingest {
            Some(i) if i - t <= DAY => {}
            _ => break 'valid,
        }
        ev.ukey = keys::key(f[1]);
        ev.ts = (t - ctx.ws) as u32;
        ev.ip = ingest_key << 24 | price as u64;
        if price > 0 {
            ev.cat = ctx.products.get(f[5]).unwrap_or(ctx.unknown_cat);
        }
    }
    buckets[part(mix(ekey), P_BITS)].push(ev);
}

#[inline(always)]
fn parse_click(f: &[&[u8]; 4], ctx: &Ctx, out: &mut Vec<Cl>) {
    if f[0].is_empty() || f[1].is_empty() {
        return;
    }
    let Some(t) = ts::parse(f[3]) else { return };
    if t < ctx.cws || t >= ctx.we {
        return;
    }
    let Some(camp) = ctx.campaigns.get(f[2]) else { return };
    out.push(Cl { ukey: keys::key(f[1]), ckey: keys::key(f[0]), ts: (t - ctx.cws) as u32, camp });
}

/// Every linked ID that is not its person's name -> the name. A bitmap in
/// front of the table answers most lookups (IDs that are names) from cache.
struct Aliases {
    keys: Vec<u64>,
    vals: Vec<u64>,
    mask: usize,
    filter: Vec<u64>,
    fmask: u64,
}

impl Aliases {
    fn from_pairs(pairs: &[(u64, u64)]) -> Self {
        let cap = (pairs.len() * 2).next_power_of_two().max(16);
        let fbits = (pairs.len() * 16).next_power_of_two().max(1 << 12);
        let mut m = Aliases {
            keys: vec![EMPTY; cap],
            vals: vec![0; cap],
            mask: cap - 1,
            filter: vec![0; fbits / 64],
            fmask: fbits as u64 - 1,
        };
        for &(k, v) in pairs {
            let h = mix(k);
            let f = h & m.fmask;
            m.filter[(f >> 6) as usize] |= 1 << (f & 63);
            let mut s = h as usize & m.mask;
            while m.keys[s] != EMPTY && m.keys[s] != k {
                s = (s + 1) & m.mask;
            }
            m.keys[s] = k;
            m.vals[s] = v;
        }
        m
    }
    /// `h` is mix(k).
    #[inline(always)]
    fn person(&self, k: u64, h: u64) -> u64 {
        let f = h & self.fmask;
        if self.filter[(f >> 6) as usize] & (1 << (f & 63)) == 0 {
            return k;
        }
        let mut s = h as usize & self.mask;
        loop {
            let x = self.keys[s];
            if x == k {
                return self.vals[s];
            }
            if x == EMPTY {
                return k;
            }
            s = (s + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn prefetch(&self, h: u64) {
        let f = h & self.fmask;
        prefetch(self.filter.as_ptr().wrapping_add((f >> 6) as usize));
        prefetch(self.keys.as_ptr().wrapping_add(h as usize & self.mask));
    }
}

/// Union-find over the links: the byte-wise smallest ID names each person.
fn build_aliases(paths: &[PathBuf]) -> Aliases {
    let bytes: usize = paths.iter().map(|p| std::fs::metadata(p).map(|m| m.len() as usize).unwrap_or(0)).sum();
    // A link line is ~21 bytes and names two IDs; most IDs appear in ~1.2 lines.
    let expect = bytes / 12 + 1024;
    let mut index = SmallIndex::new(expect * 2);
    let mut ids: Vec<u64> = Vec::with_capacity(expect);
    let mut parent: Vec<u32> = Vec::with_capacity(expect);
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        while parent[x as usize] != x {
            let gp = parent[parent[x as usize] as usize];
            parent[x as usize] = gp;
            x = gp;
        }
        x
    }
    for path in paths {
        let data = std::fs::read(path).expect("read links");
        csv::for_lines(body(&data), |line| {
            let mut f = [&b""[..]; 2];
            if !csv::split(line, &mut f) || f[0].is_empty() || f[1].is_empty() {
                return;
            }
            let mut id_of = |k: u64| -> u32 {
                let (i, new) = index.get_or_insert(k, ids.len() as u32);
                if new {
                    ids.push(k);
                    parent.push(i);
                }
                i
            };
            let a = id_of(keys::key(f[0]));
            let b = id_of(keys::key(f[1]));
            let ra = find(&mut parent, a);
            let rb = find(&mut parent, b);
            if ra != rb {
                parent[ra as usize] = rb;
            }
        });
    }
    let t_uf = Instant::now();
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
    let t_pairs = t_uf.elapsed().as_secs_f64();
    let t = Instant::now();
    let a = Aliases::from_pairs(&pairs);
    if std::env::var_os("SHOP_PROFILE").is_some() {
        eprintln!("links: parse+union until {:.3}s, best/pairs {:.3}s, table {:.3}s", T0.get().map_or(0.0, |t| t.elapsed().as_secs_f64()) - t_pairs - t.elapsed().as_secs_f64(), t_pairs, t.elapsed().as_secs_f64());
    }
    a
}
static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Growable open-addressing u64 -> u32 index (dense ids).
struct SmallIndex {
    keys: Vec<u64>,
    vals: Vec<u32>,
    mask: usize,
    len: usize,
}

impl SmallIndex {
    fn new(cap: usize) -> Self {
        let cap = cap.next_power_of_two().max(16);
        Self { keys: vec![EMPTY; cap], vals: vec![0; cap], mask: cap - 1, len: 0 }
    }
    fn reset(&mut self, expect: usize) {
        let cap = (expect * 2).next_power_of_two().max(16);
        if cap > self.keys.len() || cap * 4 < self.keys.len() {
            self.keys = vec![EMPTY; cap];
            self.vals = vec![0; cap];
            self.mask = cap - 1;
        } else {
            self.keys.fill(EMPTY);
        }
        self.len = 0;
    }
    #[inline(always)]
    fn get_or_insert(&mut self, k: u64, next: u32) -> (u32, bool) {
        let mut s = mix(k) as usize & self.mask;
        loop {
            let x = self.keys[s];
            if x == k {
                return (self.vals[s], false);
            }
            if x == EMPTY {
                self.keys[s] = k;
                self.vals[s] = next;
                self.len += 1;
                if self.len * 2 > self.keys.len() {
                    self.grow();
                }
                return (next, true);
            }
            s = (s + 1) & self.mask;
        }
    }
    #[inline(always)]
    fn get(&self, k: u64) -> Option<u32> {
        let mut s = mix(k) as usize & self.mask;
        loop {
            let x = self.keys[s];
            if x == k {
                return Some(self.vals[s]);
            }
            if x == EMPTY {
                return None;
            }
            s = (s + 1) & self.mask;
        }
    }
    fn grow(&mut self) {
        let cap = self.keys.len() * 2;
        let old_k = std::mem::replace(&mut self.keys, vec![EMPTY; cap]);
        let old_v = std::mem::replace(&mut self.vals, vec![0; cap]);
        self.mask = cap - 1;
        for (k, v) in old_k.into_iter().zip(old_v) {
            if k != EMPTY {
                let mut s = mix(k) as usize & self.mask;
                while self.keys[s] != EMPTY {
                    s = (s + 1) & self.mask;
                }
                self.keys[s] = k;
                self.vals[s] = v;
            }
        }
    }
}

struct P1 {
    buf: Vec<u8>,
    ev: Vec<Vec<Ev>>,
    cl: Vec<Cl>,
}

struct P2 {
    pe: Vec<Vec<PEv>>,
    pc: Vec<Vec<PCl>>,
    seen: Vec<u64>,
    coll: Vec<u64>,
    dups: SmallIndex,
    best: Vec<u64>,
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
    index: SmallIndex,
    persons: Vec<u64>,
    eidx: Vec<u32>,
    eoff: Vec<u32>,
    coff: Vec<u32>,
    evs: Vec<u64>,
    cls: Vec<C3>,
    catsum: Vec<u64>,
    touched: Vec<u32>,
    ccount: Vec<u32>,
    cpos: Vec<u32>,
    active: Vec<u32>,
    /// (campaigns + organic) x days x [sessions, purchasing, revenue, assisted]
    acc: Vec<[i64; 4]>,
    rows: UserRows,
    prof: [f64; 4],
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

    /// One person: events evs[e_lo..e_hi] sorted by ts, clicks cls[c_lo..c_hi]
    /// sorted by (ts, click_id). Event ts are relative to window_start, click
    /// ts to window_start - 7 days.
    fn person(&mut self, ctx: &Ctx, person: u64, e_lo: usize, e_hi: usize, c_lo: usize, c_hi: usize) {
        let organic = ctx.camp_ids.len();
        let nd = ctx.ndays;
        let (mut lo, mut hi) = (c_lo, c_lo);
        let mut cur_day = i64::MIN;
        let (mut ds, mut de, mut dr) = (0i64, 0i64, 0i64);
        let mut k = e_lo;
        while k < e_hi {
            let start = (self.evs[k] >> 32) as i64;
            let day = (ctx.ws + start).div_euclid(DAY);
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
                let w = self.evs[k];
                let t = (w >> 32) as i64;
                let price = (w >> 8) as u32 & 0xFF_FFFF;
                let cat = w as u32 & 0xFF;
                if t - last > GAP {
                    break;
                }
                last = t;
                n += 1;
                if price > 0 {
                    rev += price as i64;
                    let c = cat as usize;
                    if self.catsum[c] == 0 {
                        self.touched.push(cat);
                    }
                    self.catsum[c] += price as u64;
                }
                k += 1;
            }
            ds += 1;
            de += n;
            dr += rev;

            // Click times in the event frame: click.ts - REACH.
            let start_c = start + REACH;
            while hi < c_hi && self.cls[hi].ts as i64 <= start_c {
                let camp = self.cls[hi].camp;
                self.add(camp);
                hi += 1;
            }
            while lo < hi && (self.cls[lo].ts as i64) < start {
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
    let _ = T0.set(t0);
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
    let event_files = open_files(&list_csv(&data.join("events")));
    let click_files = open_files(&list_csv(&data.join("clicks")));
    let link_paths = list_csv(&data.join("links"));
    let event_chunks = chunks(&event_files);
    let click_chunks = chunks(&click_files);
    let n_event_chunks = event_chunks.len();
    let event_bytes: usize = event_files.iter().map(|f| f.len).sum();
    let click_bytes: usize = click_files.iter().map(|f| f.len).sum();
    // Rows are ~78 (events) and ~47 (clicks) bytes; reserve with headroom.
    let est_events = event_bytes / 70 + 1024;
    let est_clicks = click_bytes / 40 + 1024;
    lap("setup");

    // Phase 1: parse.
    let (p1, aliases) = thread::scope(|scope| {
        #[cfg(unix)]
        {
            let all: Vec<&InFile> = event_files.iter().chain(click_files.iter()).collect();
            scope.spawn(move || fadvise_all(&all));
        }
        let links = scope.spawn(|| { let t = Instant::now(); let a = build_aliases(&link_paths); if profile { eprintln!("links built in {:.3}s ({} aliases)", t.elapsed().as_secs_f64(), a.keys.iter().filter(|&&k| k != EMPTY).count()); } a });
        let p1 = pool(
            threads,
            n_event_chunks + click_chunks.len(),
            || P1 {
                buf: vec![0u8; CHUNK + 8192],
                ev: (0..P).map(|_| Vec::with_capacity(est_events / threads / P * 5 / 4)).collect(),
                cl: Vec::with_capacity(est_clicks / threads * 5 / 4),
            },
            |st, t| {
                if t < n_event_chunks {
                    let (fi, a, b) = event_chunks[t];
                    let slice = load_chunk(&event_files[fi], a, b, &mut st.buf);
                    csv::scan::<7>(slice, |f| parse_event(f, &ctx, &mut st.ev));
                } else {
                    let (fi, a, b) = click_chunks[t - n_event_chunks];
                    let slice = load_chunk(&click_files[fi], a, b, &mut st.buf);
                    csv::scan::<4>(slice, |f| parse_click(f, &ctx, &mut st.cl));
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
        CLICK_SLICES * nw + P,
        || P2 {
            pe: (0..Q).map(|_| Vec::with_capacity(est_events / threads / Q * 5 / 4)).collect(),
            pc: (0..Q).map(|_| Vec::with_capacity(est_clicks / threads / Q * 5 / 4)).collect(),
            seen: Vec::new(),
            coll: Vec::new(),
            dups: SmallIndex::new(1024),
            best: Vec::new(),
            prof: [0.0; 4],
        },
        |st, t| {
            // Click routing first (big tasks), then the event_id partitions.
            if t < CLICK_SLICES * nw {
                let all = &p1[t / CLICK_SLICES].cl;
                let k = t % CLICK_SLICES;
                let cl = &all[all.len() * k / CLICK_SLICES..all.len() * (k + 1) / CLICK_SLICES];
                const AHEAD: usize = 8;
                for i in 0..cl.len() {
                    if i + AHEAD < cl.len() {
                        aliases.prefetch(mix(cl[i + AHEAD].ukey));
                    }
                    let c = cl[i];
                    let person = aliases.person(c.ukey, mix(c.ukey));
                    st.pc[part(mix(person), Q_BITS)].push(PCl { person, ckey: c.ckey, ts: c.ts, camp: c.camp });
                }
                return;
            }
            let t = t - CLICK_SLICES * nw;
            let tt = Instant::now();
            let n: usize = p1.iter().map(|w| w.ev[t].len()).sum();
            if n == 0 {
                return;
            }
            let bits = (n * 16).next_power_of_two().max(64);
            let fmask = bits as u64 - 1;
            st.seen.clear();
            st.seen.resize(bits / 64, 0);
            st.coll.clear();
            st.coll.resize(bits / 64, 0);
            for w in &p1 {
                for e in &w.ev[t] {
                    let f = mix(e.ekey) & fmask;
                    let (word, bit) = ((f >> 6) as usize, 1u64 << (f & 63));
                    st.coll[word] |= st.seen[word] & bit;
                    st.seen[word] |= bit;
                }
            }
            st.prof[0] += tt.elapsed().as_secs_f64();
            let tt = Instant::now();
            st.dups.reset(n / 8);
            st.best.clear();
            let emit = |st_pe: &mut Vec<Vec<PEv>>, e: &Ev| {
                let price = e.ip & PRICE_MASK;
                if price == PRICE_INVALID {
                    return;
                }
                let h = mix(e.ukey);
                let person = aliases.person(e.ukey, h);
                st_pe[part(if person == e.ukey { h } else { mix(person) }, Q_BITS)].push(PEv {
                    person,
                    w: (e.ts as u64) << 32 | price << 8 | e.cat as u64,
                });
            };
            let mut dup_refs: Vec<(u32, u32)> = Vec::new();
            for (wi, w) in p1.iter().enumerate() {
                let evs = &w.ev[t];
                const AHEAD: usize = 8;
                for j in 0..evs.len() {
                    if j + AHEAD < evs.len() {
                        aliases.prefetch(mix(evs[j + AHEAD].ukey));
                    }
                    let e = &evs[j];
                    let f = mix(e.ekey) & fmask;
                    if st.coll[(f >> 6) as usize] & (1u64 << (f & 63)) == 0 {
                        emit(&mut st.pe, e);
                    } else {
                        let (slot, new) = st.dups.get_or_insert(e.ekey, dup_refs.len() as u32);
                        if new {
                            dup_refs.push((wi as u32, j as u32));
                        } else {
                            let (bw, bj) = dup_refs[slot as usize];
                            if e.ip < p1[bw as usize].ev[t][bj as usize].ip {
                                dup_refs[slot as usize] = (wi as u32, j as u32);
                            }
                        }
                    }
                }
            }
            for &(wi, j) in &dup_refs {
                emit(&mut st.pe, &p1[wi as usize].ev[t][j as usize]);
            }
            st.prof[1] += tt.elapsed().as_secs_f64();
        },
    );
    if profile {
        let mut tot = [0.0; 4];
        for w in &p2 {
            for i in 0..4 {
                tot[i] += w.prof[i];
            }
        }
        eprintln!("dedup cpu: bitmap {:.3} emit {:.3}", tot[0], tot[1]);
    }
    drop(p1);
    lap("dedup");

    // Phase 3: group by person, sessions, attribution, aggregation.
    let ncamp = ctx.camp_ids.len();
    // Largest person partitions first, so no thread is left with a big tail.
    let mut order: Vec<usize> = (0..Q).collect();
    order.sort_by_key(|&q| std::cmp::Reverse(p2.iter().map(|w| w.pe[q].len() + w.pc[q].len()).sum::<usize>()));
    let p3 = pool(
        threads,
        Q,
        || P3 {
            index: SmallIndex::new(1024),
            persons: Vec::new(),
            eidx: Vec::new(),
            eoff: Vec::new(),
            coff: Vec::new(),
            evs: Vec::new(),
            cls: Vec::new(),
            catsum: vec![0; ctx.cat_names.len()],
            touched: Vec::new(),
            ccount: vec![0; ncamp],
            cpos: vec![0; ncamp],
            active: Vec::new(),
            acc: vec![[0; 4]; (ncamp + 1) * ctx.ndays],
            rows: UserRows::default(),
            prof: [0.0; 4],
        },
        |st, t| {
            let q = order[t];
            let tt = Instant::now();
            let ne: usize = p2.iter().map(|w| w.pe[q].len()).sum();
            if ne == 0 {
                return;
            }
            // Dense person ids and counts.
            st.index.reset(ne / 8);
            st.persons.clear();
            st.eidx.clear();
            st.eoff.clear();
            for w in &p2 {
                for e in &w.pe[q] {
                    let (i, new) = st.index.get_or_insert(e.person, st.persons.len() as u32);
                    if new {
                        st.persons.push(e.person);
                        st.eoff.push(0);
                    }
                    st.eoff[i as usize] += 1;
                    st.eidx.push(i);
                }
            }
            let np = st.persons.len();
            // Exclusive prefix sums, then scatter (eoff ends as each start).
            let mut run = 0u32;
            for c in st.eoff.iter_mut() {
                let x = *c;
                *c = run;
                run += x;
            }
            st.eoff.push(run);
            st.evs.clear();
            st.evs.resize(ne, 0);
            {
                let mut cursor: Vec<u32> = st.eoff[..np].to_vec();
                let mut k = 0;
                for w in &p2 {
                    for e in &w.pe[q] {
                        let i = st.eidx[k] as usize;
                        k += 1;
                        st.evs[cursor[i] as usize] = e.w;
                        cursor[i] += 1;
                    }
                }
            }
            // Clicks of persons that have events.
            st.coff.clear();
            st.coff.resize(np + 1, 0);
            st.eidx.clear();
            let mut nc = 0usize;
            for w in &p2 {
                for c in &w.pc[q] {
                    match st.index.get(c.person) {
                        Some(i) => {
                            st.coff[i as usize] += 1;
                            st.eidx.push(i);
                            nc += 1;
                        }
                        None => st.eidx.push(NONE),
                    }
                }
            }
            let mut run = 0u32;
            for c in st.coff[..np].iter_mut() {
                let x = *c;
                *c = run;
                run += x;
            }
            st.coff[np] = run;
            st.cls.clear();
            st.cls.resize(nc, C3::default());
            {
                let mut cursor: Vec<u32> = st.coff[..np].to_vec();
                let mut k = 0;
                for w in &p2 {
                    for c in &w.pc[q] {
                        let i = st.eidx[k];
                        k += 1;
                        if i != NONE {
                            let i = i as usize;
                            st.cls[cursor[i] as usize] = C3 { ckey: c.ckey, ts: c.ts, camp: c.camp };
                            cursor[i] += 1;
                        }
                    }
                }
            }
            st.prof[0] += tt.elapsed().as_secs_f64();
            let tt = Instant::now();
            for p in 0..np {
                let (e_lo, e_hi) = (st.eoff[p] as usize, st.eoff[p + 1] as usize);
                let (c_lo, c_hi) = (st.coff[p] as usize, st.coff[p + 1] as usize);
                st.evs[e_lo..e_hi].sort_unstable();
                if c_hi - c_lo > 1 {
                    st.cls[c_lo..c_hi].sort_unstable_by(|a, b| a.ts.cmp(&b.ts).then_with(|| keys::cmp(a.ckey, b.ckey)));
                }
                let person = st.persons[p];
                st.person(&ctx, person, e_lo, e_hi, c_lo, c_hi);
            }
            st.prof[1] += tt.elapsed().as_secs_f64();
        },
    );
    if profile {
        let mut tot = [0.0; 4];
        for w in &p3 {
            for i in 0..4 {
                tot[i] += w.prof[i];
            }
        }
        eprintln!("session cpu: group {:.3} sweep {:.3}", tot[0], tot[1]);
    }
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
