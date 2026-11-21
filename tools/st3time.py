"""Interleaved timing of pipeline variants with XL_TIMING=1 phase laps.

Usage: python3 st3time.py DATA_DIR ROUNDS VARIANTS_FILE
VARIANTS_FILE lines:  label  binary  [ENV=VAL ...]   (first line = baseline)
Reports, per variant: median wall, ev3a lap, total lap, scatter/compact/runs
thread-ms, and the median per-round difference to the baseline.
"""
import os
import random
import re
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
    variants.append((parts[0], parts[1], dict(p.split('=', 1) for p in parts[2:])))

KEYS = ['wall', 'parse', 'ev3a', 'total', 'scatter', 'compact', 'runs']
ncpu = str(os.cpu_count())
res = {v[0]: {k: [] for k in KEYS} for v in variants}
for r in range(rounds):
    order = list(range(len(variants)))
    random.shuffle(order)
    for i in order:
        label, binary, env = variants[i]
        e = dict(os.environ, DATA_DIR=data, NCPU=ncpu, XL_TIMING='1')
        e.update(env)
        with tempfile.TemporaryDirectory() as out:
            e['OUTPUT_DIR'] = out
            t = time.perf_counter()
            p = subprocess.run([binary], env=e, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
            dt = time.perf_counter() - t
        row = {k: None for k in KEYS}
        if p.returncode == 0:
            row['wall'] = dt * 1000
            for k in ('parse', 'ev3a', 'total'):
                m = re.search(r'\b' + k + r'=([0-9.]+)', p.stderr)
                if m:
                    row[k] = float(m.group(1))
            for k in ('scatter', 'compact', 'runs'):
                m = re.search(r'prof \(thread-ms\):.*?\b' + k + r'=([0-9.]+)', p.stderr)
                if m:
                    row[k] = float(m.group(1))
        for k in KEYS:
            res[label][k].append(row[k])

base = variants[0][0]
print(f"{'variant':14s} " + ' '.join(f'{k:>16s}' for k in KEYS))
for label, _, _ in variants:
    cells = []
    for k in KEYS:
        xs = [x for x in res[label][k] if x is not None]
        if not xs:
            cells.append(f"{'-':>16s}")
            continue
        d = [a - b for a, b in zip(res[label][k], res[base][k]) if a is not None and b is not None]
        md = st.median(d) if d else 0.0
        cells.append(f'{st.median(xs):7.1f}({md:+6.1f})')
    print(f'{label:14s} ' + ' '.join(f'{c:>16s}' for c in cells))
print('cells: median ms (median paired diff vs first variant); n rounds =', rounds)
