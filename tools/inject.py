"""Copy a dataset and inject malformed / odd event and click lines.

Usage: python3 inject.py SRC_DIR DST_DIR SEED
Adds, at random positions of random event and click files, lines with too few
or too many commas, quotes in odd places, empty lines, and valid-looking lines
cut short, so that parsers' fast paths meet them at every alignment.
"""
import os
import random
import shutil
import sys

src, dst, seed = sys.argv[1], sys.argv[2], int(sys.argv[3])
rnd = random.Random(seed)
if os.path.exists(dst):
    shutil.rmtree(dst)
shutil.copytree(src, dst)


def variants(line, ncols):
    f = line.split(',')
    out = []
    for k in range(0, ncols + 2):           # 0..ncols+1 fields joined -> wrong comma counts
        if k != ncols:
            out.append(','.join(f[:k]) if k <= len(f) else line + ',x' * (k - len(f)))
    out.append('')                           # empty line
    out.append(line[: rnd.randrange(1, len(line))])   # cut short
    out.append(line.replace(',', ',"', 1))   # stray quote
    out.append('"' + line)                   # quote at start only
    out.append(line + '"')                   # quote at end only
    out.append(line.replace(',', ',,', 1))   # extra empty field
    return out


for sub, ncols in (('events', 7), ('clicks', 4)):
    d = os.path.join(dst, sub)
    for name in sorted(os.listdir(d)):
        p = os.path.join(d, name)
        lines = open(p, encoding='utf-8').read().split('\n')
        if len(lines) < 3:
            continue
        body = lines[1:-1] if lines[-1] == '' else lines[1:]
        n_inj = max(1, len(body) // 2000)
        for _ in range(n_inj):
            base = body[rnd.randrange(len(body))]
            bad = rnd.choice(variants(base, ncols))
            body.insert(rnd.randrange(len(body) + 1), bad)
        open(p, 'w', encoding='utf-8', newline='\n').write('\n'.join([lines[0]] + body) + '\n')
print('injected into', dst)
