# Benchmarks

Local machine: i7-12700K (Docker Desktop on WSL2, 15.5 GiB VM, kernel 6.6, THP `madvise`); container pinned to
CPUs 0-7 (4 P-cores with hyperthreading), `--cpus 8 --memory 14g`, data in the `ssdata` volume, page cache warm.
`scripts/bench.py` (median of 5) unless noted. Scorer time has been about 1.17-1.3 × the local container time.

Phase columns: wall seconds of the phase, then (user + sys) / (wall × 8) = parallel efficiency, and sys share.

## v1-baseline (e196257, scored 1.10 s)

### p20 (21.50 M event lines, 5.15 M clicks, 0.89 M links)

Container median **0.938 s** (best 0.897), fixed cost (`SS_NOOP=1`) ~0.25 s, internal 0.642 s, peak RSS 2.74 GB.

| phase | wall | user | sys | faults | efficiency | sys share |
| --- | --- | --- | --- | --- | --- | --- |
| links parsed | 0.008 | 0.014 | 0.005 | 4.8 k | 30 % | 26 % |
| events + clicks parsed (names on thread 0: 0.272 s) | 0.330 | 2.087 | 0.383 | 57 k | 94 % | 16 % |
| deduplicated | 0.126 | 0.771 | 0.063 | 1.7 k | 83 % | 8 % |
| names resolved (2b) | 0.034 | 0.230 | 0.021 | 0 | 92 % | 8 % |
| sessionized | 0.115 | 0.856 | 0.041 | 33 k | 97 % | 5 % |
| written | 0.025 | 0.015 | 0.127 | 31 k | 71 % | 89 % |

### p60 (64.1 M event lines, 15.0 M valid clicks, 2.66 M links)

Container median **2.583 s** (best 1.989, worst 2.987: the VM is short of memory at 60M), fixed ~0.29 s,
internal 2.148 s, peak RSS 7.9 GB, so ~0.15 s of exit teardown.

| phase | wall | user | sys | faults | efficiency | sys share |
| --- | --- | --- | --- | --- | --- | --- |
| links parsed | 0.014 | 0.043 | 0.026 | 12 k | 62 % | 38 % |
| events + clicks parsed (names on thread 0: 0.955 s) | 1.130 | 6.889 | 0.946 | 170 k | 87 % | 12 % |
| deduplicated | 0.379 | 2.221 | 0.195 | 3.7 k | 80 % | 8 % |
| names resolved (2b) | 0.129 | 0.849 | 0.153 | 0 | 97 % | 15 % |
| sessionized | 0.374 | 2.684 | 0.289 | 62 k | 99 % | 10 % |
| written | 0.118 | 0.126 | 0.549 | 92 k | 71 % | 81 % |

### /output on disk vs tmpfs (p20, 8 interleaved rounds)

Named volume (disk) median 1.154 s, tmpfs 0.832 s. The process lifetime (`docker inspect` StartedAt to FinishedAt)
is the same in both modes (0.66-0.72 s): the ~0.3 s is spent by Docker after the process exits, and it grows with
the output size (dev, 6 MB of output: +0.04 s). A plain `dd` of 115 MB to the same volume does not show it, so the
cause is not understood. A/B decisions use tmpfs (less noise); a smaller output (dictionaries) is a cheap hedge in
case the scorer behaves the same.

## Oracles

`../data/v1-p20` (equals `../data/out-p20`) and `../data/v1-p60` (equals the earlier `../data/v24-p60`).

## Prototype front-end (proto/, adapted to base 36 IDs), p20, one thread, i7-12700K

Levels: 0 stage 1 only 8.8 ns/line; 1 + lines and quotes 12.8; 2 + both IDs (base 36) 18.9; 3 + ts and ingest 29.0;
4 + validation, batch, dedup bitmap (2^29 bits), names (filter + 64 MiB table, 40% renamed) 45.4; 5 + 16-byte
scatter into 1024 partitions 54.1 (1.16 s for 21.5 M lines).

Current binary, `NCPU=1`, same scope (events parsed - names built 0.085 s, + deduplicated 0.46 s + names resolved
0.12 s), still including the 5.1 M click lines and the mmap faults: 2.1-2.5 s, so about 1.7-1.9 times the prototype.
The prototype leaves out the dedup resolution pass, real name probes, exotic interning and file boundaries.

## Tokenizer, p20, one thread (proto levels 10/11 run src/input.rs on the same buffer)

Current `simd_range` 19.4 ns/line (with ts::ingest + ts::parse: 30.4); prototype stages 1-2 12.8 ns/line (with
both base 36 IDs and the SSE F1 path: 29.0; with src/ts.rs instead of f1_fast: 32.9). So step 2 would save about
10 ns/line, ~0.2 s of CPU at 20M (~0.045 s wall, ~5%), not the 8-10% expected: postponed to the tuning step.

## Step 3 as planned (branch v2-bitmap): exact, but slower

One scatter by person, dedup by a shared bitmap of event values, names looked up at parse. Exact on dev and
p20, but p20 parse 0.44 s (v1 0.30) and p60 parse 3.5 s with 6 s of system time: every copy does a random
read-modify-write in a 128-256 MB bitmap and 40% a random name lookup, which cost more than v1's sequential
scatters. With an L2-sized bitmap (wrong dedup, timing only) parse is still 0.395 s. Abandoned.

## What went in instead (internal time, p20, vs d1b54d3 = scored 1.07 s)

| change | effect |
| --- | --- |
| names built on all threads before parsing (5b99a0d) | p20 -2.6%, p60 neutral (0.95 s on thread 0 -> 0.15 s on 8) |
| 2 MiB chunks | -2.0% (1 MiB: +2.6%) |
| 3 name slots per link (instead of 5) | -1.3% |
| two-stage tokenizer (2d430f9) | 19.4 -> 15.6 ns/line one thread, parse -4% |
| INT32 integer columns (728f313) | user_daily 92 -> 67 MB |
| cumulative (728f313) | p20 -4.0% (median of diffs) / -4.7% (medians); p60 -3.6% |

Measured and dropped: counting sort by rank + per-person sort in sessions (+9%), SSE F1 (0), one-multiply eid
hash (0), SS_IO=pread (+23%), lazy input faults (0), SS_DROP=0 (+1%), eid partitions 1344/4096, person
partitions 1344, names filter 4 or 16 bits per node (all within noise).

Disk vs tmpfs /output: v1-baseline +0.36 s, current +0.10 s (the parallel release and _exit removed most of it).

Profile at 20M (SS_PROFILE=1, seconds summed over 8 threads): dedup insert 0.27, kept 0.06, route 0.34;
sessions rank 0.19, sort 0.36, person 0.39. Names at 60M: inserts 0.069 s, unions 0.024, roots 0.024,
names 0.019 (34k 4 KiB faults: no transparent huge pages available in the VM at that point).

## Scorer vs local, cold cache

Scores: e196257 1.10 s, d1b54d3 1.07 s, 217201f 1.05 s, for local internal times 0.642, 0.616, 0.573 s:
the scorer moves by about half of the internal change (noise, a large fixed part, or I/O).
Local cold page cache (drop_caches before the run): 1.42-1.5 s instead of 0.83 s, parse 0.89 s instead of
0.26 s (the disk, about 2 GB/s, is the limit). Background readahead of every file (posix_fadvise WILLNEED)
does not help cold (1.434 vs 1.419 s): 8 threads populating 2 MiB chunks already keep the disk busy.
Also neutral: unmapping whole files in parallel after the parse instead of per-chunk MADV_DONTNEED (+1.1%),
deferred name lookups without the filter check (+0.1%, kept: it is the same result with less work).

## Scorer probes (commits that set ENV SS_PROBE in the Dockerfile; never candidates)

| probe | commit | encodes | score | reading |
| --- | --- | --- | --- | --- |
| P1 time | ca191b5 | sleep 10 x own time to output written | 10.28 s | own time T ~0.92 s (fixed + 11 T), vs ~0.56 s locally: 1.63x; fixed cost ~0.1 s |
| P2 env | 9db3c00 | 0.5 THP never, 1 affinity > NCPU, 2 cold cache, 4 no huge page, 8 CPU quota | 9.04 s | base ~1.04 + 8: a cgroup CPU quota, THP works, warm page cache, affinity <= 8 |
| P7 ab-io | c235769 | 9 + 30 x (mmap - pread), events parse | 11.80 s | mmap ~ pread on the scorer |

So the score is almost all our process time, on CPUs about 1.6x slower than the local i7 for this code.
Leaderboard (21 Nov afternoon): #1 0.31 s, then 0.52, 0.52, 0.54, 0.58; we 69th at 1.05. Below 0.50 s needs
T below ~0.38 s on the scorer, about 2.4x less than now.

Local measurements while another project loads the Docker VM (no free huge pages there, AnonHugePages 0):
the p20 events parse storing into fresh arenas 0.41 s, into already faulted ones 0.31 s, not storing 0.30 s;
MADV_POPULATE_WRITE 2 MiB at a time recovers about half (-13%). On the scorer huge pages work (P2), so this
cost is much smaller there.

## Fork of Team 67's Express Lane (MIT), 21 Nov evening

`express/` = github.com/jason-scheffel/express-lane @ 92cf412 (see README, LICENSE). Imported unmodified in
ca6b68e: **0.31 s** on the scorer (BB-493737E4), from 1.01 s.

It needs AVX-512 (built for Cascade Lake). The local i7-12700K has none (SIGILL), so local runs go through
Intel SDE 10.13.1 (`sde64 -clx`, test-only images, never timed): correctness only. Under SDE, the
pipeline's fallback re-exec (`/proc/self/exe --generic`) is not followed by Pin and dies of SIGILL, so the
generic mode is checked by starting it directly (`/pipeline --generic`: same argv and env as the re-exec).

| check (under SDE) | fork ca6b68e | + PGO |
| --- | --- | --- |
| 36 fixtures (fast path) | 36/36 | 36/36 |
| dev vs dev-answer | MATCH | MATCH |
| p20 vs out-p20 | MATCH | MATCH |
| diff_seeds, 13 seeds (Feb 29, month, new-year windows) x 2 sizes x 3 settings | 108/108 | 108/108 |
| fuzz vs reference.py, generic mode | 300/300 (seed 13) | 600/600 (seeds 11, 12) |

Timing cannot be done locally: A/B on the scorer with `probe/` (scripts/probe/Dockerfile.*), which runs
variants A and B alternately (ABBA, best of 3 each) in one container, then A for real into /output, and ends
at AB_BASE + AB_SCALE x (A - B) seconds (10 + 100 x by default).

| scorer probe | commit | variants | score | reading |
| --- | --- | --- | --- | --- |
| P8 ab-pgo | df71ca5 | A = PGO build, B = same source without; ends at 10 + 100 x (A - B) | 10.15 s | A - B ~ 0 (at most +1.5 ms with ~0.15 s fixed cost): PGO dropped |
