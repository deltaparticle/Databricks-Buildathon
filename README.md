# Shopping Sessions

A single static Rust binary that turns the dirty click-stream CSVs in `/data` into the two exact daily
Parquet tables `/output/user_daily/` and `/output/campaign_daily/`, following `rules.md`.

## Credits and layout

- **`express/`: the pipeline in the image.** It is based on Team 67's Express Lane
  (github.com/jason-scheffel/express-lane, commit 92cf412), used under the MIT License; Team 67's
  copyright notice is kept in `LICENSE`. It infers a compact ID representation from the input, checks
  it as it goes, and re-runs itself in an exact generic mode when the data breaks its assumptions.
  Our changes to it are in the git history after the commit that imports it. It needs AVX-512
  (built for Cascade Lake, the scorer's CPU).
- **Profile-guided optimization, tried and dropped.** `scripts/pgo/` builds an instrumented pipeline
  (feature `pgo-gen`) and merges its profile on never-scored generator seeds; a scorer A/B (BENCH.md,
  P8) found no measurable gain, so the image is built without it.
- **`probe/`: scorer A/B measurements** (`scripts/probe/`, used only in probe commits, never a candidate).
- **`src/`, `generic/`: our own earlier pipeline** (scored 1.01 s, commit 14b4668), described below.
  It is kept in the repository and its tests, but it is not in the image.

## Our earlier pipeline (`src/`)

All phases run on `NCPU` threads over memory-mapped inputs split into line-aligned chunks.

1. **Lookups.** `manifest.json`, `products.csv` and `campaigns.csv`. Category ids follow byte-wise name order
   (with `unknown`), so top-category ties resolve to the smallest id.
2. **IDs as exact 64-bit keys.** An ID of 1 to 10 bytes in `[0-9A-Z_a-z]` is packed 6 bits per byte, so
   integer order is byte order. Any other ID is interned and compared by its bytes.
3. **Links.** Parsed in parallel, then a union-find names every person by the byte-wise smallest of their
   IDs. This runs on one thread while the others parse events and clicks.
4. **Events.** Each well-formed copy is validated on its own fields (window, type, product, price, late
   check) and scattered by `event_id`. Clicks are validated (form of `ts`, campaign known) and kept.
5. **Dedup.** Per `event_id` partition, the copy with the smallest `ingest_ts` wins. Surviving kept copies
   are mapped to their person and scattered by person, and so are clicks.
6. **Sessions.** Per person partition: sort by time, split on gaps over 30 minutes, credit the latest click
   in the 7 days up to the session start (ties to the smallest `click_id`), and count assists from a
   sliding window of per-campaign counts.
7. **Output.** Each thread writes its own `user_daily` file; `campaign_daily` is merged and written once.

## Build and run

```sh
docker buildx build --platform linux/amd64 --load -t shopping-sessions:latest .
python scripts/docker_run.py DATA_DIR OUTPUT_DIR     # scorer flags; also scripts/docker_run.sh
```

Without Docker: `cargo build --release`, then `target/release/shopping-sessions DATA_DIR OUTPUT_DIR`
(or set `DATA_DIR` / `OUTPUT_DIR`).

## Testing

- `cargo test --release`: unit tests for timestamps, the CSV dialect, chunking and key order.
- `python scripts/run_fixtures.py fixtures -- <binary>`: the 36 edge-case fixtures.
- `python scripts/compare.py OUT dev-answer`: exact diff against the published dev answer.
- `reference/reference.py`: an independent, literal, slow implementation of `rules.md`, used as an oracle.
- `python scripts/diff_seeds.py GENERATE BINARY ROOT`: pipeline vs reference over many generator seeds
  (including windows holding Feb 29 2028, Mar 1 and New Year) and thread/partition settings.
- `python scripts/fuzz.py BINARY`: thousands of small adversarial datasets, pipeline vs reference, with
  random thread counts, chunk sizes down to 1 byte and partition counts.

Knobs for testing only: `NCPU`, `SS_CHUNK` (chunk bytes), `SS_PARTS` (partition count).
