# Shopping Sessions

**Shipped pipeline (`src/`, Rust, built by the root Dockerfile):** our own independent
implementation of `rules.md`, written by our team. It parses every CSV with a SIMD line
scanner, deduplicates events by exact ID, joins linked IDs with union-find, sessionizes
per person and writes both Parquet tables with a hand-written writer. IDs are treated as
opaque byte strings (short ones packed into integers, long ones interned), as scorer v5
requires.

CI (`.github/workflows/own.yml`) runs the 36 fixtures, the official v5 dev answer
(`tools/dev-answer/`), an exact 20M cross-check, phase timings and container timing. The
official v5 generator source is in `tools/generator`.

**History:** earlier submissions used other engines (an adaptation of
[Dremix10/shopping-sessions-fast](https://github.com/Dremix10/shopping-sessions-fast) and
Team 67's MIT [Express Lane](https://github.com/jason-scheffel/express-lane)). After the
organizers' rule that entries must be the team's own work, main ships only our own code;
that earlier work, with its credits, is kept on the `archive/pre-v5` branch. See
[`attempts/`](attempts/README.md).


## Design

1. **Parse (parallel).** Every CSV is memory-mapped and cut into ~4 MB newline-aligned chunks (the dialect never puts a newline inside a value, so any `\n` is a safe cut). Worker threads parse events and clicks; events are routed into 256 partitions by a hash of `event_id`. Links are joined on their own thread at the same time (union-find; the byte-wise smallest ID names the person).
2. **Dedup (parallel over event_id partitions).** Open-addressing table keeps the copy with the smallest `ingest_ts`; validity of each copy is decided at parse time, so an invalid earliest copy still wins and drops the event. Survivors and clicks are re-routed into 256 partitions by person.
3. **Sessions (parallel over person partitions).** Sort by (person, ts), split at gaps over 30 min, credit each session to the latest click within 7 days (ties: smallest `click_id`) with a two-pointer sweep, and count assisting campaigns with a sliding window over distinct campaigns. Each worker writes its own `user_daily` Parquet file.

IDs of 1–10 chars from `[0-9A-Z_a-z]` are packed into order-preserving 60-bit integers; anything else is interned, so comparisons stay exact.

## Checks

```sh
cargo build --release
python3 scripts/run_fixtures.py fixtures -- target/release/shopping   # 36/36
DATA_DIR=dev OUTPUT_DIR=out target/release/shopping && python3 scripts/compare.py out dev-answer
```

`SHOP_PROFILE=1` prints phase timings to stderr.

## Practice-size data on a low-memory machine

The official generator holds the whole dataset in RAM before writing. `tools/gen-lowmem` is a copy that generates users in batches and appends each batch to the part files: every row lands in the same file as with the official generator, only the line order inside a file differs (which the rules make irrelevant; verified on the 1M dev set: identical files up to line order, and the pipeline output still matches the dev answer).

```powershell
cargo build --release --manifest-path tools/gen-lowmem/Cargo.toml
tools/gen-lowmem/target/release/generate --seed 42 --events 20000000 --clicks 5000000 --threads 8 --output D:/data/p20
powershell -File tools/bench.ps1 -Data D:\data\p20 -Exe target\release\shopping.exe -Runs 3
```
