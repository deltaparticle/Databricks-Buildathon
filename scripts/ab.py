#!/usr/bin/env python3
"""
A/B timing of two images, interleaved so background noise hits both alike.
    ab.py DATA IMAGE_A IMAGE_B [--rounds 6]
Each round runs A then B (order alternates) once; prints both medians, the median
of the per-round differences, and the per-phase medians of each image.
"""
import argparse
import re
import statistics

from bench import run

PHASE = re.compile(r"\+\s*([0-9.]+)s .*\] (links parsed|events parsed|deduplicated|names resolved|sessionized|written)")


TOTAL = re.compile(r"\[\s*([0-9.]+)s .*\] written")


def phases(log):
    out = {m.group(2): float(m.group(1)) for m in PHASE.finditer(log)}
    m = TOTAL.search(log)
    if m:
        out["internal"] = float(m.group(1))  # process start to output written
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("data")
    ap.add_argument("a")
    ap.add_argument("b")
    ap.add_argument("--rounds", type=int, default=6)
    ap.add_argument("--cpus", default="8")
    ap.add_argument("--mem", default="14g")
    ap.add_argument("--cpuset", default="0-7")
    ap.add_argument("--env", nargs="*", default=[])
    ap.add_argument("--output", choices=["volume", "tmpfs"], default="tmpfs")
    args = ap.parse_args()
    walls = {args.a: [], args.b: []}
    logs = {args.a: [], args.b: []}
    diffs = []
    for r in range(args.rounds):
        order = [args.a, args.b] if r % 2 == 0 else [args.b, args.a]
        got = {}
        for image in order:
            # IMAGE@K=V,K2=V2 runs IMAGE with extra environment variables
            args.image, _, extra = image.partition("@")
            env = args.env + [kv for kv in extra.split(",") if kv]
            wall, log = run(args, env)
            got[image] = wall
            walls[image].append(wall)
            logs[image].append(phases(log))
        diffs.append(got[args.b] - got[args.a])
        print(f"round {r + 1}: A {got[args.a]:.3f}s  B {got[args.b]:.3f}s  B-A {diffs[-1]:+.3f}s", flush=True)
    for image, tag in ((args.a, "A"), (args.b, "B")):
        per = {k: statistics.median(p[k] for p in logs[image] if k in p) for k in logs[image][0]}
        print(f"{tag} {image}: median {statistics.median(walls[image]):.3f}s  " +
              "  ".join(f"{k} {v:.3f}" for k, v in per.items()))
    print(f"median B-A {statistics.median(diffs):+.3f}s")
    ia = [p["internal"] for p in logs[args.a] if "internal" in p]
    ib = [p["internal"] for p in logs[args.b] if "internal" in p]
    if ia and ib:
        di = [b - a for a, b in zip(ia, ib)]
        print(f"internal: A {statistics.median(ia):.3f}s  B {statistics.median(ib):.3f}s  median B-A {statistics.median(di):+.3f}s "
              f"({100 * statistics.median(di) / statistics.median(ia):+.1f}%)")


if __name__ == "__main__":
    main()
