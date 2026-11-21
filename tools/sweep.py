"""Interleaved timing sweep of pipeline variants on one machine.

Usage: python3 sweep.py DATA_DIR ROUNDS VARIANTS_FILE
VARIANTS_FILE lines:  label  binary  [ENV=VAL ...]
The first line is the baseline. Every round runs every variant once in a
shuffled order; the report gives each variant's median wall time and the median
of its per-round difference to the baseline (negative = faster).
"""
import os
import random
import statistics as st
import subprocess
import sys
import tempfile
import time

data, rounds, vfile = sys.argv[1], int(sys.argv[2]), sys.argv[3]
variants = []
for line in open(vfile):
    line = line.split('#')[0].strip()
    if not line:
        continue
    parts = line.split()
    env = dict(p.split('=', 1) for p in parts[2:])
    variants.append((parts[0], parts[1], env))

ncpu = str(os.cpu_count())
times = {v[0]: [] for v in variants}
fails = {v[0]: 0 for v in variants}
for r in range(rounds):
    order = list(range(len(variants)))
    random.shuffle(order)
    for i in order:
        label, binary, env = variants[i]
        e = dict(os.environ, DATA_DIR=data, NCPU=ncpu)
        e.update(env)
        with tempfile.TemporaryDirectory() as out:
            e['OUTPUT_DIR'] = out
            t = time.perf_counter()
            p = subprocess.run([binary], env=e, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            dt = time.perf_counter() - t
        if p.returncode != 0:
            fails[label] += 1
            times[label].append(None)
        else:
            times[label].append(dt)

base = variants[0][0]
rows = []
for label, _, env in variants:
    ok = [x for x in times[label] if x is not None]
    if not ok:
        rows.append((9e9, label, 'FAILED', ''))
        continue
    diffs = [a - b for a, b in zip(times[label], times[base]) if a is not None and b is not None]
    md = st.median(diffs) if diffs else 0.0
    rows.append((md, label, f'median {st.median(ok)*1000:7.1f} ms', f'diff vs base {md*1000:+6.1f} ms ({md/st.median([x for x in times[base] if x])*100:+5.1f}%) n={len(ok)} fails={fails[label]}'))
for md, label, a, b in sorted(rows):
    print(f'{label:28s} {a}  {b}')
