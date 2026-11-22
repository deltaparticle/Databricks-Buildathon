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
mod sessions;
mod stats;
mod ts;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use arena::Scatter;

use events::{Context, EventCopy};
use output::Column;
use sessions::{Acc, PersonClick, PersonEvent, Shape, NO_CATEGORY};
use ts::DAY;

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn main() {
    if std::env::var_os("SS_NOOP").is_some() {
        std::process::exit(0); // measures the fixed cost of starting and stopping the container
    }
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
    let chunk_size = env_usize("SS_CHUNK").unwrap_or(4 << 20).max(1);

    let window = lookups::read_manifest(&data);
    let products = lookups::Products::read(&data);
    let campaigns = lookups::Campaigns::read(&data);
    let event_input = input::Input::open(&data.join("events"));
    let click_input = input::Input::open(&data.join("clicks"));
    let link_input = input::Input::open(&data.join("links"));
    let event_bytes = event_input.bytes() as usize;
    let click_bytes = click_input.bytes() as usize;
    let simd = input::use_simd();
    let est_copies = event_bytes / 75 + 1;
    let eid_parts = env_usize("SS_PARTS").unwrap_or((est_copies / 16_384).clamp(1, 4096)).max(1);
    let person_parts = env_usize("SS_PARTS").unwrap_or((est_copies / 32_768).clamp(1, 4096)).max(1);
    log(&format!(
        "threads {threads}, simd {simd}, events {event_bytes} B in {} files, clicks {click_bytes} B, {} link files, partitions {eid_parts}/{person_parts}",
        event_input.files.len(),
        link_input.files.len()
    ));

    let cx = Context {
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

    // Phase 1b: names on thread 0, events and clicks everywhere.
    let event_chunks = event_input.chunks(chunk_size);
    let click_chunks = click_input.chunks(chunk_size);
    let total_chunks = event_chunks.len() + click_chunks.len();
    // Arena room per thread: thread 0 spends a while on the names, so the others
    // each parse about 1/(threads-1) of the data. Untouched room costs nothing.
    let busy = threads.saturating_sub(1).max(1);
    let est_clicks = click_bytes / 45 + 1;
    let event_room = est_copies * 13 / 10 / busy;
    let click_room = est_clicks * 13 / 10 / busy;
    let next = AtomicUsize::new(0);
    let names_cell: std::sync::OnceLock<links::Names> = std::sync::OnceLock::new();
    let mut event_parts: Vec<Scatter<EventCopy>> = Vec::with_capacity(threads);
    let mut person_clicks: Vec<Scatter<PersonClick>> = Vec::with_capacity(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (event_chunks, click_chunks, event_input, click_input, next, cx, link_pairs, names_cell) =
                    (&event_chunks, &click_chunks, &event_input, &click_input, &next, &cx, &link_pairs, &names_cell);
                s.spawn(move || {
                    if t == 0 {
                        let started = std::time::Instant::now();
                        let _ = names_cell.set(links::build(link_pairs));
                        eprintln!("           names built in {:.3}s on thread 0", started.elapsed().as_secs_f64());
                    }
                    let mut parts: Scatter<EventCopy> = Scatter::new(eid_parts, est_copies / threads, event_room);
                    let mut clicks: Scatter<PersonClick> = Scatter::new(person_parts, est_clicks / threads, click_room);
                    let mut buf = Vec::new();
                    let mut pending_events = events::Pending::new();
                    let mut pending_clicks = events::Pending::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= total_chunks {
                            break;
                        }
                        if i < event_chunks.len() {
                            // Names, once built, are resolved right away.
                            let names = if std::env::var_os("SS_RESOLVE_AT_PARSE").is_some() { names_cell.get() } else { None };
                            event_input.parse::<7, _>(&event_chunks[i], &mut buf, simd, &mut |f| {
                                events::event_fields(f, cx, names, &mut pending_events, &mut parts)
                            });
                        } else {
                            // Clicks come last; they go straight to their person's partition.
                            let names = names_cell.wait();
                            click_input.parse::<4, _>(&click_chunks[i - event_chunks.len()], &mut buf, simd, &mut |f| {
                                events::click_fields(f, cx, names, &mut pending_clicks, &mut clicks)
                            });
                        }
                    }
                    if let Some(names) = names_cell.get() {
                        events::flush_events(names, &mut pending_events, &mut parts);
                        events::flush_clicks(names, &mut pending_clicks, &mut clicks);
                    }
                    (parts, clicks)
                })
            })
            .collect();
        for h in handles {
            let (parts, clicks) = h.join().unwrap();
            event_parts.push(parts);
            person_clicks.push(clicks);
        }
    });
    let names = names_cell.get().expect("names were built");
    log(&format!(
        "events parsed: {} copies, {} valid clicks",
        event_parts.iter().map(Scatter::total).sum::<usize>(),
        person_clicks.iter().map(Scatter::total).sum::<usize>()
    ));

    // Phase 2: dedup and route to person partitions.
    let person_room = est_copies * 13 / 10 / threads;
    let next = AtomicUsize::new(0);
    let person_events: Vec<Scatter<PersonEvent>> =
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let (event_parts, next) = (&event_parts, &next);
                    s.spawn(move || {
                        let mut evs: Scatter<PersonEvent> = Scatter::new(person_parts, est_copies / threads, person_room);
                        let mut table = Vec::new();
                        let mut kept = Vec::new();
                        let mut slices: Vec<&[EventCopy]> = Vec::with_capacity(2 * event_parts.len());
                        loop {
                            let p = next.fetch_add(1, Ordering::Relaxed);
                            if p >= eid_parts {
                                break;
                            }
                            slices.clear();
                            slices.extend(event_parts.iter().flat_map(|t| t.part(p)));
                            sessions::dedup(&slices, names, &mut table, &mut kept, &mut evs);
                        }
                        evs
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
    log("deduplicated");

    // Phase 3: sessions per person partition, biggest first.
    let first_day = window.start.div_euclid(DAY);
    let last_day = (window.end - 1).div_euclid(DAY);
    let shape = Shape {
        first_day,
        days: (last_day - first_day + 1).max(1) as usize,
        campaigns: campaigns.ids.len(),
    };
    let mut order: Vec<(usize, usize)> = (0..person_parts)
        .map(|q| {
            let n = person_events.iter().map(|t| t.len(q)).sum::<usize>()
                + person_clicks.iter().map(|t| t.len(q)).sum::<usize>();
            (n, q)
        })
        .collect();
    order.sort_unstable_by(|a, b| b.cmp(a));
    let next = AtomicUsize::new(0);
    let accs: Vec<Acc> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (order, person_events, person_clicks, next) = (&order, &person_events, &person_clicks, &next);
                s.spawn(move || {
                    let mut acc = Acc::new(shape);
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&(_, q)) = order.get(i) else { break };
                        acc.partition(
                            person_events.iter().flat_map(|t| t.part(q)),
                            person_clicks.iter().flat_map(|t| t.part(q)),
                        );
                    }
                    acc
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    log("sessionized");

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
                let mut sessions = Column::int64s("sessions", rows);
                let mut events = Column::int64s("events", rows);
                let mut revenue = Column::int64s("revenue_cents", rows);
                let mut top = Column::optional_strings("top_category", rows);
                for i in 0..rows {
                    user.push_str_with(|out| key::bytes_into(acc.user[i], out));
                    day.push_i32(acc.day[i]);
                    sessions.push_i64(acc.sessions[i]);
                    events.push_i64(acc.events[i]);
                    revenue.push_i64(acc.revenue[i]);
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
            for (total, part) in stats.iter_mut().zip(&acc.stats) {
                for k in 0..4 {
                    total[k] += part[k];
                }
            }
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
        eprintln!(
            "user_daily rows {}, campaign_daily rows {rows}, sessions {user_sessions}, revenue {user_revenue}",
            accs.iter().map(|a| a.user.len()).sum::<usize>(),
        );
        output::write(&campaign_dir.join("part-00000.parquet"), &[ids, channels, days, sess, purch, rev, assisted])
            .expect("writing campaign_daily");
    });
    let peak = std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmHWM")).map(|l| l.split_whitespace().skip(1).collect::<Vec<_>>().join(" ")))
        .unwrap_or_default();
    log(&format!("written; peak RSS {peak}"));
    std::process::exit(0);
}
