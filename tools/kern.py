"""Interleaved native vs container timing of pipeline variants on one machine.

Usage: python3 kern.py DATA_DIR ROUNDS VARIANTS_FILE
VARIANTS_FILE lines (first line = baseline):
  label  native  ./binary        [ENV=VAL ...]
  label  docker  image           [ENV=VAL ...]   [--flag ...]  (args starting with -- go to docker create)
Every round runs every variant once in shuffled order. Reported per variant:
  wall  = outer wall (native: process spawn to exit; docker: docker start -a to return)
  dock  = docker State.StartedAt -> FinishedAt (docker only)
  inner = the pipeline's own total= (XL_TIMING=1), ms since main()
  exit  = the pipeline's 'exit at' ms (after the final release, right before _exit)
Medians, and median per-round difference of wall (and dock) vs the baseline.
"""
import os
import random
import re
import statistics as st
import subprocess
import sys
import tempfile
import time
from datetime import datetime

data, rounds, vfile = sys.argv[1], int(sys.argv[2]), sys.argv[3]
ncpu = str(os.cpu_count())
BASEFLAGS = ("--network none --read-only --tmpfs /tmp:rw,size=64m,mode=1777 --cap-drop ALL "
             "--security-opt no-new-privileges --pids-limit 4096 --memory 14g --memory-swap 14g "
             "--user 1000:1000").split() + ["--cpus", ncpu]

variants = []
for line in open(vfile):
    line = line.split('#')[0].strip()
    if not line:
        continue
    p = line.split()
    env = dict(x.split('=', 1) for x in p[3:] if '=' in x and not x.startswith('--'))
    flags = [x for x in p[3:] if x.startswith('--')]
    fl = []
    for f in flags:
        fl.extend(f.split('=', 1) if f.startswith('--security-opt=') else [f])
    variants.append((p[0], p[1], p[2], env, fl))


def ts(x):
    x = x.strip().rstrip('Z')
    if '.' in x:
        a, b = x.split('.')
        x = a + '.' + b[:6]
    else:
        x += '.0'
    return datetime.strptime(x, '%Y-%m-%dT%H:%M:%S.%f')


def parse_err(s):
    m = re.search(r'total=([0-9.]+)', s)
    e = re.search(r'exit at ([0-9.]+)', s)
    return (float(m.group(1)) if m else None, float(e.group(1)) if e else None)


def run(v):
    label, kind, target, env, flags = v
    with tempfile.TemporaryDirectory() as out:
        os.chmod(out, 0o777)
        if kind == 'native':
            e = dict(os.environ, DATA_DIR=data, NCPU=ncpu, OUTPUT_DIR=out, XL_TIMING='1')
            e.update(env)
            t = time.perf_counter()
            p = subprocess.run([target], env=e, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            dt = time.perf_counter() - t
            inner, ex = parse_err(p.stderr.decode(errors='replace'))
            return (p.returncode, dt, None, inner, ex)
        envs = []
        for k, val in dict({'NCPU': ncpu, 'XL_TIMING': '1'}, **env).items():
            envs += ['-e', f'{k}={val}']
        cid = subprocess.run(['docker', 'create'] + BASEFLAGS + flags + ['-v', f'{data}:/data:ro', '-v', f'{out}:/output'] + envs + [target],
                             capture_output=True, text=True).stdout.strip()
        t = time.perf_counter()
        p = subprocess.run(['docker', 'start', '-a', cid], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        dt = time.perf_counter() - t
        sf = subprocess.run(['docker', 'inspect', '-f', '{{.State.StartedAt}} {{.State.FinishedAt}} {{.State.ExitCode}}', cid], capture_output=True, text=True).stdout.split()
        subprocess.run(['docker', 'rm', cid], capture_output=True)
        dock = (ts(sf[1]) - ts(sf[0])).total_seconds() if len(sf) >= 2 else None
        inner, ex = parse_err(p.stderr.decode(errors='replace'))
        rc = int(sf[2]) if len(sf) >= 3 else 99
        return (rc, dt, dock, inner, ex)


res = {v[0]: [] for v in variants}
for r in range(rounds):
    order = list(range(len(variants)))
    random.shuffle(order)
    for i in order:
        res[variants[i][0]].append(run(variants[i]))

base = variants[0][0]


def med(xs):
    xs = [x for x in xs if x is not None]
    return st.median(xs) if xs else float('nan')


bw = [x[1] for x in res[base]]
bd = [x[2] for x in res[base]]
print(f'{"variant":22s} {"wall":>8s} {"dwall":>8s} {"dock":>8s} {"ddock":>8s} {"inner":>8s} {"exit":>8s} fails')
for label, kind, *_ in variants:
    rr = res[label]
    fails = sum(1 for x in rr if x[0] != 0)
    w = [x[1] for x in rr]
    d = [x[2] for x in rr]
    dw = med([a - b for a, b in zip(w, bw)])
    dd = med([a - b for a, b in zip(d, bd) if a is not None and b is not None])
    print(f'{label:22s} {med(w)*1e3:8.1f} {dw*1e3:+8.1f} {med(d)*1e3:8.1f} {dd*1e3:+8.1f} {med([x[3] for x in rr]):8.1f} {med([x[4] for x in rr]):8.1f} {fails}')
