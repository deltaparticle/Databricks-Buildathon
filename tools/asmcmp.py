"""Compare the machine code of two builds, ignoring addresses.

Usage: python asmcmp.py OBJDUMP old_binary new_binary

Disassembles both, splits by function, and replaces every address and
address-like immediate with a placeholder, so that code which only moved
compares equal. Prints functions that differ, exist only on one side, or
"IDENTICAL CODE".
"""
import re
import subprocess
import sys

objdump, a, b = sys.argv[1:4]

FUNC = re.compile(r'^[0-9a-f]+ <(.+)>:$')
INSN = re.compile(r'^\s*[0-9a-f]+:\s*(.*)$')
HEX = re.compile(r'0x[0-9a-f]+')
SYMREF = re.compile(r'<[^>]*>')
ADDR = re.compile(r'\b[0-9a-f]{5,}\b')


def funcs(path):
    out = subprocess.run([objdump, '-d', '--no-show-raw-insn', '-M', 'intel', path],
                         capture_output=True, text=True, check=True).stdout
    res, cur, body = {}, None, []
    for line in out.splitlines():
        m = FUNC.match(line)
        if m:
            if cur is not None:
                res.setdefault(cur, []).append(tuple(body))
            cur, body = m.group(1), []
            continue
        m = INSN.match(line)
        if m and cur is not None:
            ins = m.group(1).split('#')[0].strip()
            ins = SYMREF.sub('<S>', ins)
            ins = HEX.sub('X', ins)
            ins = ADDR.sub('A', ins)
            body.append(ins)
    if cur is not None:
        res.setdefault(cur, []).append(tuple(body))
    return res


def norm_name(n):
    # strip Rust legacy hashes (h0123456789abcdef) and LLVM clone suffixes
    n = re.sub(r'17h[0-9a-f]{16}E', '', n)
    n = re.sub(r'\.\d+$', '', n)  # local-symbol counters renumber when other code is removed
    return n


fa = {}
for k, v in funcs(a).items():
    fa.setdefault(norm_name(k), []).extend(v)
fb = {}
for k, v in funcs(b).items():
    fb.setdefault(norm_name(k), []).extend(v)

only_a = sorted(set(fa) - set(fb))
only_b = sorted(set(fb) - set(fa))
diff = [k for k in sorted(set(fa) & set(fb)) if sorted(fa[k]) != sorted(fb[k])]
na = sum(len(x) for v in fa.values() for x in v)
nb = sum(len(x) for v in fb.values() for x in v)
print(f'instructions: old {na}, new {nb}; functions: old {len(fa)}, new {len(fb)}')
for k in only_a:
    print('ONLY OLD', k, sum(len(x) for x in fa[k]))
for k in only_b:
    print('ONLY NEW', k, sum(len(x) for x in fb[k]))
for k in diff:
    print('DIFFERS', k, sum(len(x) for x in fa[k]), '->', sum(len(x) for x in fb[k]))
if not (only_a or only_b or diff):
    print('IDENTICAL CODE')
