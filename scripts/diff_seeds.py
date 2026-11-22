#!/usr/bin/env python3
"""
Generates datasets with the published generator for many seeds and sizes, runs the
reference and the pipeline (with several NCPU / SS_PARTS / SS_CHUNK settings) on
each, and diffs the outputs exactly.

Usage: diff_seeds.py GENERATE BINARY DATA_ROOT [--find-windows N] [--sizes ...] [--seeds ...]
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent / "reference"))

import reference  # noqa: E402
from run_fixtures import SCHEMAS, load_actual  # noqa: E402


def generate(gen, seed, events, clicks, out):
    if (out / "manifest.json").exists():
        return
    subprocess.run([gen, "--seed", str(seed), "--events", str(events), "--clicks", str(clicks), "--output", str(out)],
                   check=True, capture_output=True)


def rows(folder, table):
    df = load_actual(Path(folder) / table, SCHEMAS[table])
    return sorted(df.rows(), key=lambda row: tuple("" if v is None else str(v) for v in row))


def interesting_seeds(gen, root, count):
    """Seeds whose window holds Feb 29, Mar 1-2 or New Year."""
    picks = []
    probe = root / "probe"
    for seed in range(100, 5000):
        shutil.rmtree(probe, ignore_errors=True)
        generate(gen, seed, 1000, 250, probe)
        m = json.loads((probe / "manifest.json").read_text())
        start, end = m["window_start"][:10], m["window_end"][:10]
        tags = []
        for y in ("2025", "2026", "2027", "2028"):
            for md, tag in (("02-29", "feb29"), ("03-01", "mar1"), ("01-01", "newyear")):
                day = f"{y}-{md}"
                if start <= day < end and (md != "02-29" or y == "2028"):
                    tags.append(f"{tag}{y}")
        if tags:
            picks.append((seed, start, tags))
        if len(picks) >= count and any("feb29" in t for p in picks for t in p[2]):
            break
    shutil.rmtree(probe, ignore_errors=True)
    return picks


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("generate")
    ap.add_argument("binary")
    ap.add_argument("root")
    ap.add_argument("--seeds", type=int, nargs="*", default=[2, 3, 5, 7, 11, 13, 17])
    ap.add_argument("--find-windows", type=int, default=6)
    ap.add_argument("--events", type=int, nargs="*", default=[20_000, 200_000])
    args = ap.parse_args()
    root = Path(args.root)
    root.mkdir(parents=True, exist_ok=True)

    seeds = [(s, "") for s in args.seeds]
    if args.find_windows:
        for seed, start, tags in interesting_seeds(args.generate, root, args.find_windows):
            print(f"seed {seed}: window from {start} holds {','.join(tags)}")
            seeds.append((seed, ",".join(tags)))

    settings = [dict(NCPU="8"), dict(NCPU="1", SS_PARTS="1"), dict(NCPU="3", SS_PARTS="5", SS_CHUNK="4096")]
    failures = 0
    for seed, tags in seeds:
        for events in args.events:
            data = root / f"s{seed}-e{events}"
            generate(args.generate, seed, events, max(events // 4, 1), data)
            ref = root / f"ref-s{seed}-e{events}"
            if not (ref / "campaign_daily").exists():
                reference.run(data, ref)
            expected = {t: rows(ref, t) for t in SCHEMAS}
            for setting in settings:
                out = root / "out"
                shutil.rmtree(out, ignore_errors=True)
                p = subprocess.run([args.binary, str(data), str(out)], env=dict(os.environ, **setting),
                                   capture_output=True, text=True)
                verdict = "MATCH"
                if p.returncode != 0:
                    verdict = f"EXIT {p.returncode}: {p.stderr[-500:]}"
                else:
                    for t in SCHEMAS:
                        got = rows(out, t)
                        if got != expected[t]:
                            sa, sb = set(expected[t]), set(got)
                            verdict = f"DIFFER {t}: ref-only {len(sa - sb)}, pipeline-only {len(sb - sa)}; " \
                                      f"e.g. {sorted(sa - sb, key=str)[:2]} / {sorted(sb - sa, key=str)[:2]}"
                            break
                if verdict != "MATCH":
                    failures += 1
                print(f"seed {seed:>5} events {events:>8} {tags:<14} {setting}: {verdict}", flush=True)
    print("ALL MATCH" if not failures else f"{failures} FAILURES")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
