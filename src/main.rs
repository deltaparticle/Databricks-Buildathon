//! Shopping Sessions: dirty click-stream CSV in, two exact daily Parquet tables out.
//!
//! Phases (all on NCPU threads):
//!   1. links parsed; then one thread builds the person names while the others
//!      parse events (scattered by event_id) and clicks
//!   2. per event_id partition: dedup on the smallest ingest_ts, then the kept,
//!      surviving copies go to their person's partition; clicks likewise
//!   3. per person partition: sessions, credit, assists, daily aggregates
//!   4. Parquet

mod arena;
mod csv;
mod events;
mod hash;
mod input;
mod key;
mod links;
mod lookups;
mod output;
mod probe;
mod prof;
mod sessions;
mod stats;
mod ts;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use arena::{Scatter, Words3};

use events::{Context, EventCopy};
use output::Column;
use sessions::{Acc, PersonClick, PersonEvent, Shape, NO_CATEGORY};
use ts::DAY;

/// Hands the whole run to the generic binary next to this one.
fn run_generic(args: &[String]) -> ! {
    let exe = std::env::current_exe().expect("current_exe");
    let generic = exe.with_file_name(format!("shopping-sessions-generic{}", std::env::consts::EXE_SUFFIX));
    if stats::enabled() {
        eprintln!("running {}", generic.display());
    }
    let mut command = std::process::Command::new(&generic);
    command.args(&args[1..]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        panic!("exec {}: {error}", generic.display());
    }
    #[cfg(not(unix))]
    {
        let status = command.status().expect("running the generic binary");
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn main() {
    if std::env::var_os("SS_NOOP").is_some() {
        std::process::exit(0); // measures the fixed cost of starting and stopping the container
    }
    let started = std::time::Instant::now();
    let probe_mode = probe::mode();
    let mut stats = stats::Stats::new();
    let mut log = |what: &str| stats.log(what);
    let args: Vec<String> = std::env::args().collect();
    let (data, out) = if args.len() >= 3 {
        (PathBuf::from(&args[1]), PathBuf::from(&args[2]))
    } else {
        (
            PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into())),
            PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into())),
        )
    };
    let threads = env_usize("NCPU")
        .or_else(|| std::thread::available_parallelism().ok().map(usize::from))
        .unwrap_or(4)
        .max(1);
    let chunk_size = env_usize("SS_CHUNK").unwrap_or(2 << 20).max(1);

    let window = lookups::read_manifest(&data);
    let products = lookups::Products::read(&data);
    let campaigns = lookups::Campaigns::read(&data);
    let event_input = input::Input::open(&data.join("events"));
    let click_input = input::Input::open(&data.join("clicks"));
    let link_input = input::Input::open(&data.join("links"));
    let cold = if probe_mode.as_deref() == Some("env") { event_input.probe_cold() } else { 0.0 };
    let event_bytes = event_input.bytes() as usize;
    let click_bytes = click_input.bytes() as usize;
    let simd = input::use_simd();
    let est_copies = event_bytes / 75 + 1;
    let eid_parts = env_usize("SS_EID_PARTS")
        .or_else(|| env_usize("SS_PARTS"))
        .unwrap_or((est_copies / 8_192).clamp(1, 4096))
        .max(1);
    let person_parts = env_usize("SS_PERSON_PARTS")
        .or_else(|| env_usize("SS_PARTS"))
        .unwrap_or((est_copies / 32_768).clamp(1, 4096))
        .max(1);
    log(&format!(
        "threads {threads}, simd {simd}, events {event_bytes} B in {} files, clicks {click_bytes} B, {} link files, partitions {eid_parts}/{person_parts}",
        event_input.files.len(),
        link_input.files.len()
    ));

    // The compact records need every kept ts within 2^32 ms of base and at most
    // 32 categories; anything else (never produced by the generator) goes to the
    // generic binary, which has none of these limits.
    let base = window.start - events::WEEK;
    let compact_ok = window.end > window.start
        && window.end - base <= events::C_TS as i64
        && products.categories.len() <= events::MAX_CATEGORIES;
    if !compact_ok || std::env::var_os("SS_FORCE_GENERIC").is_some() {
        run_generic(&args);
    }
    let cx = Context {
        base,
        window_start: window.start,
        window_end: window.end,
        products: &products,
        campaigns: &campaigns,
    };

    // Phase 1a: links.
    let link_chunks = link_input.chunks(chunk_size);
    let next = AtomicUsize::new(0);
    let link_pairs: Vec<Vec<(u64, u64)>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (link_chunks, link_input, next) = (&link_chunks, &link_input, &next);
                s.spawn(move || {
                    let mut out = Vec::new();
                    let mut buf = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(c) = link_chunks.get(i) else { break };
                        link_input.parse::<2, _>(c, &mut buf, simd, &mut |f| events::link_fields(f, &mut out));
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    log(&format!("links parsed: {} pairs", link_pairs.iter().map(Vec::len).sum::<usize>()));
    probe::mark("links");

    // Phase 1b: the names on every thread (SS_NAMES=serial: on thread 0 while the
    // others start parsing), then events and clicks everywhere.
    let names_cell: std::sync::OnceLock<links::Names> = std::sync::OnceLock::new();
    let serial_names = std::env::var("SS_NAMES").as_deref() == Ok("serial");
    if !serial_names {
        let _ = names_cell.set(links::build_parallel(&link_pairs, threads));
        log("names built");
        probe::mark("names");
    }
    let event_chunks = event_input.chunks(chunk_size);
    let click_chunks = click_input.chunks(chunk_size);
    let total_chunks = event_chunks.len() + click_chunks.len();
    // Arena room per thread: with serial names thread 0 spends a while on them, so
    // the others each parse about 1/(threads-1) of the data. Untouched room costs nothing.
    let busy = if serial_names { threads.saturating_sub(1).max(1) } else { threads };
    // Benchmark and probe only: the events parse alone, once on every thread.
    let parse_round = |input: &input::Input, chunks: &[input::Chunk], dry: bool| -> f64 {
        let room = est_copies * 13 / 10 / threads;
        let next = AtomicUsize::new(0);
        let started = std::time::Instant::now();
        std::thread::scope(|s| {
            for _ in 0..threads {
                let (next, cx) = (&next, &cx);
                s.spawn(move || {
                    let mut parts = events::Copies {
                        compact: Words3::new(eid_parts, room),
                        full: Scatter::new(eid_parts, est_copies / threads / 16, room / 8),
                    };
                    let mut buf = Vec::new();
                    let mut sum = 0u64;
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(c) = chunks.get(i) else { break };
                        if dry {
                            input.parse::<7, _>(c, &mut buf, simd, &mut |f| sum = sum.wrapping_add(events::event_fields_dry(f, cx)));
                        } else {
                            input.parse::<7, _>(c, &mut buf, simd, &mut |f| events::event_fields(f, cx, &mut parts));
                        }
                    }
                    std::hint::black_box(sum);
                    parts.compact.finish();
                    parts.full.finish();
                    std::hint::black_box(parts.compact.total() + parts.full.total());
                });
            }
        });
        started.elapsed().as_secs_f64()
    };
    if let (Some(rounds), true) = (env_usize("SS_PARSEBENCH"), std::env::var_os("SS_PARSEBENCH_REUSE").is_some()) {
        // the same, each thread storing into the same (already faulted) arenas every round
        let room = est_copies * 13 / 10 / threads;
        let mut copies: Vec<events::Copies> = (0..threads)
            .map(|_| events::Copies { compact: Words3::new(eid_parts, room), full: Scatter::new(eid_parts, est_copies / threads / 16, room / 8) })
            .collect();
        let mut best = f64::MAX;
        for _ in 0..rounds.max(1) {
            let next = AtomicUsize::new(0);
            let started = std::time::Instant::now();
            std::thread::scope(|s| {
                for parts in copies.iter_mut() {
                    let (next, cx, event_chunks, event_input) = (&next, &cx, &event_chunks, &event_input);
                    s.spawn(move || {
                        parts.compact.reset();
                        parts.full.reset();
                        let mut buf = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(c) = event_chunks.get(i) else { break };
                            event_input.parse::<7, _>(c, &mut buf, simd, &mut |f| events::event_fields(f, cx, parts));
                        }
                        parts.compact.finish();
                        parts.full.finish();
                    });
                }
            });
            best = best.min(started.elapsed().as_secs_f64());
        }
        eprintln!("parsebench: events parsed in {best:.4}s (best of {rounds}, reused arenas)");
        exit_now();
    }
    if let Some(rounds) = env_usize("SS_PARSEBENCH") {
        // the fastest of `rounds` rounds is printed and the process ends
        let dry = std::env::var_os("SS_PARSEBENCH_DRY").is_some();
        let best = (0..rounds.max(1)).map(|_| parse_round(&event_input, &event_chunks, dry)).fold(f64::MAX, f64::min);
        eprintln!("parsebench: events parsed in {best:.4}s (best of {rounds})");
        exit_now();
    }
    if probe_mode.as_deref() == Some("ab-io") {
        // the same parse from memory maps (A) and with pread (B), alternated
        std::env::set_var("SS_IO", "pread");
        let pread_input = input::Input::open(&data.join("events"));
        std::env::remove_var("SS_IO");
        let pread_chunks = pread_input.chunks(chunk_size);
        let (mut a, mut b) = (f64::MAX, f64::MAX);
        for _ in 0..2 {
            a = a.min(parse_round(&event_input, &event_chunks, false));
            b = b.min(parse_round(&pread_input, &pread_chunks, false));
        }
        probe::set_ab(a, b);
    }
    if probe_mode.as_deref() == Some("ab-thp") {
        // the same parse with arenas advised for huge pages (A) and against them (B)
        let (mut a, mut b) = (f64::MAX, f64::MAX);
        for _ in 0..2 {
            std::env::remove_var("SS_THP");
            a = a.min(parse_round(&event_input, &event_chunks, false));
            std::env::set_var("SS_THP", "0");
            b = b.min(parse_round(&event_input, &event_chunks, false));
        }
        std::env::remove_var("SS_THP");
        probe::set_ab(a, b);
    }
    if probe_mode.as_deref() == Some("ab-prefault") {
        // the same parse with arenas faulted page by page (A) and 2 MiB at a time (B)
        let (mut a, mut b) = (f64::MAX, f64::MAX);
        for _ in 0..2 {
            arena::set_prefault(false);
            a = a.min(parse_round(&event_input, &event_chunks, false));
            arena::set_prefault(true);
            b = b.min(parse_round(&event_input, &event_chunks, false));
        }
        arena::set_prefault(false);
        probe::set_ab(a, b);
    }
    if probe_mode.as_deref() == Some("ab-scatter") {
        // the same parse storing its copies (A) and only decoding them (B), alternated
        let (mut a, mut b) = (f64::MAX, f64::MAX);
        for _ in 0..2 {
            a = a.min(parse_round(&event_input, &event_chunks, false));
            b = b.min(parse_round(&event_input, &event_chunks, true));
        }
        probe::set_ab(a, b);
    }
    let est_clicks = click_bytes / 45 + 1;
    let event_room = est_copies * 13 / 10 / busy;
    let click_room = est_clicks * 13 / 10 / busy;
    let next = AtomicUsize::new(0);
    let mut event_parts: Vec<events::Copies> = Vec::with_capacity(threads);
    let mut person_clicks: Vec<Words3> = Vec::with_capacity(threads);
    let mut deferred_clicks: Vec<Words3> = Vec::with_capacity(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (event_chunks, click_chunks, event_input, click_input, next, cx, link_pairs, names_cell) =
                    (&event_chunks, &click_chunks, &event_input, &click_input, &next, &cx, &link_pairs, &names_cell);
                s.spawn(move || {
                    if t == 0 && names_cell.get().is_none() {
                        let started = std::time::Instant::now();
                        let _ = names_cell.set(links::build(link_pairs));
                        if stats::enabled() {
                            eprintln!("           names built in {:.3}s on thread 0", started.elapsed().as_secs_f64());
                        }
                    }
                    let mut parts = events::Copies {
                        compact: Words3::new(eid_parts, event_room),
                        full: Scatter::new(eid_parts, est_copies / threads / 16, event_room / 8),
                    };
                    let mut clicks = Words3::new(person_parts, click_room);
                    // by names-table region, made once the names exist
                    let mut deferred: Option<Words3> = None;
                    let mut buf = Vec::new();
                    let mut pending_clicks = events::ClickPending::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= total_chunks {
                            break;
                        }
                        if i < event_chunks.len() {
                            event_input.parse::<7, _>(&event_chunks[i], &mut buf, simd, &mut |f| {
                                events::event_fields(f, cx, &mut parts)
                            });
                        } else {
                            // Clicks come last; they go straight to their person's partition.
                            let names = names_cell.wait();
                            let deferred = deferred.get_or_insert_with(|| Words3::new(names.regions(), click_room / 2));
                            click_input.parse::<4, _>(&click_chunks[i - event_chunks.len()], &mut buf, simd, &mut |f| {
                                events::click_fields(f, cx, names, &mut pending_clicks, &mut clicks, deferred)
                            });
                        }
                    }
                    let names = names_cell.wait();
                    let mut deferred = deferred.unwrap_or_else(|| Words3::new(names.regions(), 0));
                    events::flush_clicks(names, &mut pending_clicks, &mut clicks, &mut deferred);
                    parts.compact.finish();
                    parts.full.finish();
                    clicks.finish();
                    deferred.finish();
                    (parts, clicks, deferred)
                })
            })
            .collect();
        for h in handles {
            let (parts, clicks, deferred) = h.join().unwrap();
            event_parts.push(parts);
            person_clicks.push(clicks);
            deferred_clicks.push(deferred);
        }
    });
    let names = names_cell.get().expect("names were built");
    // The parsed ranges' pages were released by the parsing threads: unmapping is cheap now.
    drop((event_input, click_input, link_input));
    log(&format!(
        "events parsed: {} copies, {} valid clicks",
        event_parts.iter().map(|t| t.compact.total() + t.full.total()).sum::<usize>(),
        person_clicks.iter().chain(&deferred_clicks).map(Words3::total).sum::<usize>()
    ));
    probe::mark("parse");

    // Phase 2: dedup and route to person partitions.
    let person_room = est_copies * 13 / 10 / threads;
    let recycle = std::env::var("SS_RECYCLE").as_deref() != Ok("0");
    let next = AtomicUsize::new(0);
    let regions = names.regions();
    let stage: Vec<(Scatter<PersonEvent>, Scatter<PersonEvent>)> =
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let (event_parts, next) = (&event_parts, &next);
                    s.spawn(move || {
                        let mut evs: Scatter<PersonEvent> = Scatter::new(person_parts, est_copies / threads, person_room);
                        let mut deferred: Scatter<PersonEvent> = Scatter::new(regions, est_copies / threads / 2, person_room / 2);
                        let mut table = sessions::Table::default();
                        let mut kept = Vec::new();
                        let mut compact: Vec<&[[u64; 3]]> = Vec::with_capacity(8 * event_parts.len());
                        let mut full: Vec<&[EventCopy]> = Vec::with_capacity(2 * event_parts.len());
                        loop {
                            let p = next.fetch_add(1, Ordering::Relaxed);
                            if p >= eid_parts {
                                break;
                            }
                            compact.extend(event_parts.iter().flat_map(|t| t.compact.part(p)));
                            full.extend(event_parts.iter().flat_map(|t| t.full.part(p)));
                            sessions::dedup(&compact, &full, base, names, &mut table, &mut kept, &mut evs, &mut deferred);
                            compact.clear();
                            full.clear();
                            // Partition p is done: its blocks become this thread's
                            // (already touched) memory for person events.
                            if recycle {
                                for t in event_parts.iter() {
                                    evs.adopt(t.compact.blocks_of(p));
                                    evs.adopt(t.full.blocks_of(p));
                                }
                            }
                        }
                        deferred.finish();
                        (evs, deferred)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
    log("deduplicated");
    probe::mark("dedup");

    // Phase 2b: the deferred name lookups, one names-table region at a time.
    let (mut person_events, deferred): (Vec<Scatter<PersonEvent>>, Vec<Scatter<PersonEvent>>) = stage.into_iter().unzip();
    let next = AtomicUsize::new(0);
    let click_room = est_clicks * 13 / 10 / threads / 2;
    let resolved_clicks: Vec<Words3> = std::thread::scope(|s| {
        let handles: Vec<_> = person_events
            .iter_mut()
            .map(|evs| {
                let (deferred, deferred_clicks, next) = (&deferred, &deferred_clicks, &next);
                s.spawn(move || {
                    let mut clicks = Words3::new(person_parts, click_room);
                    loop {
                        let r = next.fetch_add(1, Ordering::Relaxed);
                        if r >= regions {
                            break;
                        }
                        sessions::resolve_deferred(deferred.iter().flat_map(|t| t.part(r)), names, evs);
                        sessions::resolve_deferred_clicks(deferred_clicks.iter().flat_map(|t| t.part(r)), names, &mut clicks);
                    }
                    evs.finish();
                    clicks.finish();
                    clicks
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    drop(deferred);
    drop(deferred_clicks);
    log("names resolved");
    probe::mark("resolve");

    // Phase 3: sessions per person partition, biggest first.
    let first_day = window.start.div_euclid(DAY);
    let last_day = (window.end - 1).div_euclid(DAY);
    let shape = Shape {
        base,
        first_day,
        days: (last_day - first_day + 1).max(1) as usize,
        campaigns: campaigns.ids.len(),
    };
    let mut order: Vec<(usize, usize)> = (0..person_parts)
        .map(|q| {
            let n = person_events.iter().map(|t| t.len(q)).sum::<usize>()
                + person_clicks.iter().chain(&resolved_clicks).map(|t| t.len(q)).sum::<usize>();
            (n, q)
        })
        .collect();
    order.sort_unstable_by(|a, b| b.cmp(a));
    let next = AtomicUsize::new(0);
    let accs: Vec<Acc> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (order, person_events, person_clicks, resolved_clicks, next) = (&order, &person_events, &person_clicks, &resolved_clicks, &next);
                s.spawn(move || {
                    // about 1 user_daily row per 9.5 event copies in the generator's data
                    let mut acc = Acc::new(shape, est_copies / 8 / threads + 1024);
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(_, q)) = order.get(i) else { break };
                        acc.partition(
                            person_events.iter().flat_map(|t| t.part(q)),
                            person_clicks.iter().chain(resolved_clicks).flat_map(|t| t.part(q)).map(PersonClick::of_words),
                        );
                    }
                    acc
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    log("sessionized");
    probe::mark("sessions");

    // Phase 4: output.
    let user_dir = out.join("user_daily");
    let campaign_dir = out.join("campaign_daily");
    std::fs::create_dir_all(&user_dir).expect("creating user_daily/");
    std::fs::create_dir_all(&campaign_dir).expect("creating campaign_daily/");
    let categories = &products.categories;
    std::thread::scope(|s| {
        for (t, acc) in accs.iter().enumerate() {
            if acc.user.is_empty() && t != 0 {
                continue;
            }
            let path = user_dir.join(format!("part-{t:05}.parquet"));
            s.spawn(move || {
                let rows = acc.user.len();
                let mut user = Column::strings("user_id", rows);
                let mut day = Column::dates("day", rows);
                let sessions = Column::integers("sessions", &acc.sessions);
                let events = Column::integers("events", &acc.events);
                let revenue = Column::integers("revenue_cents", &acc.revenue);
                let mut top = Column::optional_strings("top_category", rows);
                for i in 0..rows {
                    user.push_str_with(|out| key::bytes_into(acc.user[i], out));
                    day.push_i32(acc.day[i]);
                    match acc.top[i] {
                        NO_CATEGORY => top.push_null(),
                        c => top.push_str(&categories[c as usize]),
                    }
                }
                output::write(&path, &[user, day, sessions, events, revenue, top]).expect("writing user_daily");
            });
        }

        let mut stats = vec![[0i64; 4]; (shape.campaigns + 1) * shape.days];
        for acc in &accs {
            acc.add_stats(&mut stats);
        }
        let rows = stats.iter().filter(|st| st[0] != 0 || st[3] != 0).count();
        let mut ids = Column::strings("campaign_id", rows);
        let mut channels = Column::strings("channel", rows);
        let mut days = Column::dates("day", rows);
        let mut sess = Column::int64s("sessions", rows);
        let mut purch = Column::int64s("purchasing_sessions", rows);
        let mut rev = Column::int64s("revenue_cents", rows);
        let mut assisted = Column::int64s("assisted_sessions", rows);
        let (mut camp_sessions, mut camp_revenue) = (0i64, 0i64);
        for c in 0..=shape.campaigns {
            for d in 0..shape.days {
                let st = stats[c * shape.days + d];
                if st[0] == 0 && st[3] == 0 {
                    continue;
                }
                let (id, channel): (&[u8], &[u8]) = if c == 0 {
                    (b"organic", b"organic")
                } else {
                    (&campaigns.ids[c - 1], &campaigns.channels[c - 1])
                };
                ids.push_str(id);
                channels.push_str(channel);
                days.push_i32((first_day + d as i64) as i32);
                sess.push_i64(st[0]);
                purch.push_i64(st[1]);
                rev.push_i64(st[2]);
                assisted.push_i64(st[3]);
                camp_sessions += st[0];
                camp_revenue += st[2];
            }
        }
        let user_sessions: i64 = accs.iter().flat_map(|a| &a.sessions).sum();
        let user_revenue: i64 = accs.iter().flat_map(|a| &a.revenue).sum();
        if user_sessions != camp_sessions || user_revenue != camp_revenue {
            eprintln!("SELF-CHECK FAILED: sessions {user_sessions} vs {camp_sessions}, revenue {user_revenue} vs {camp_revenue}");
        }
        if stats::enabled() {
            eprintln!(
                "user_daily rows {}, campaign_daily rows {rows}, sessions {user_sessions}, revenue {user_revenue}",
                accs.iter().map(|a| a.user.len()).sum::<usize>(),
            );
        }
        output::write(&campaign_dir.join("part-00000.parquet"), &[ids, channels, days, sess, purch, rev, assisted])
            .expect("writing campaign_daily");
    });
    let peak = if stats::enabled() {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("VmHWM")).map(|l| l.split_whitespace().skip(1).collect::<Vec<_>>().join(" ")))
            .unwrap_or_default()
    } else {
        String::new()
    };
    log(&format!("written; peak RSS {peak}"));
    probe::mark("written");
    if let Some(mode) = &probe_mode {
        probe::finish(mode, started, cold, threads);
    }
    prof::report();
    // Give the memory back on every thread, then leave without the serial teardown.
    if std::env::var("SS_RELEASE").as_deref() != Ok("0") {
        arena::release_all(threads);
    }
    log("released");
    exit_now();
}

/// Ends the process at once: the outputs are written and closed, nothing is buffered.
fn exit_now() -> ! {
    #[cfg(target_os = "linux")]
    // SAFETY: _exit only ends the process.
    unsafe {
        libc::_exit(0)
    }
    #[cfg(not(target_os = "linux"))]
    std::process::exit(0)
}
