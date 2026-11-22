#!/usr/bin/env python3
"""
Differential fuzzer: writes many small adversarial datasets, runs the reference
(reference/reference.py, in process) and the pipeline binary on each, and diffs the
two outputs exactly. The pipeline runs with random NCPU, SS_CHUNK and SS_PARTS so
chunk boundaries and partitioning get exercised too.

Usage: fuzz.py BINARY [--cases N] [--seed S] [--keep DIR]
"""
import argparse
import os
import random
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent / "reference"))

import reference  # noqa: E402
from run_fixtures import SCHEMAS, load_actual  # noqa: E402

DAY = 86_400_000
MIN = 60_000
WEEK = 7 * DAY


def civil(ms):
    days, rest = divmod(ms, DAY)
    z = days + 719468
    era = z // 146097
    doe = z - era * 146097
    yoe = (doe - doe // 1460 + doe // 36524 - doe // 146096) // 365
    doy = doe - (365 * yoe + yoe // 4 - yoe // 100)
    mp = (5 * doy + 2) // 153
    d = doy - (153 * mp + 2) // 5 + 1
    m = mp + 3 if mp < 10 else mp - 9
    y = yoe + era * 400 + (m <= 2)
    secs = rest // 1000
    return y, m, d, secs // 3600, secs // 60 % 60, secs % 60


def f1(ms):
    y, m, d, hh, mm, ss = civil(ms)
    return f"{y:04d}-{m:02d}-{d:02d}T{hh:02d}:{mm:02d}:{ss:02d}Z"


def f2(ms, offset_min):
    y, m, d, hh, mm, ss = civil(ms + offset_min * MIN)
    sign = "-" if offset_min < 0 else "+"
    o = abs(offset_min)
    return f"{y:04d}-{m:02d}-{d:02d}T{hh:02d}:{mm:02d}:{ss:02d}{sign}{o // 60:02d}:{o % 60:02d}"


def good_ts(r, ms):
    """A valid rendering of ms (F1/F2 only for whole seconds)."""
    if ms % 1000 or r.random() < 0.35:
        z = "0" * r.choice([0, 0, 0, 1, 3, 25])
        return z + str(ms)
    if r.random() < 0.3:
        return f2(ms, r.choice([-600, -570, -60, -1, 0, 1, 59, 60, 330, 345, 720, 1439, -1439]))
    if r.random() < 0.05:
        return f2(ms, 0).replace("+00:00", "-00:00")
    return f1(ms)


def bad_ts(r, ms):
    base = f1(ms - ms % 1000)
    y, m, d, hh, mm, ss = civil(ms - ms % 1000)
    options = [
        "",
        base[:-1] + ".000Z",
        base[:-1] + "z",
        base.replace("T", "t"),
        base.replace("T", " "),
        base[:-1],
        f"{ms}.0",
        "1.79e12",
        "-5",
        "+" + str(ms),
        f"{y:04d}-13-{d:02d}T{hh:02d}:{mm:02d}:{ss:02d}Z",
        f"{y:04d}-02-30T{hh:02d}:{mm:02d}:{ss:02d}Z",
        f"2027-02-29T{hh:02d}:{mm:02d}:{ss:02d}Z",
        f"{y:04d}-{m:02d}-{d:02d}T24:{mm:02d}:{ss:02d}Z",
        f"{y:04d}-{m:02d}-{d:02d}T{hh:02d}:{mm:02d}:60Z",
        base[:-1] + "+24:00",
        base[:-1] + "+05:60",
        base[:-1] + "+0200",
        str(ms) + "\r",
        "٢٥٠",
        "２５０",
        "9" * 21,
        " " + str(ms),
    ]
    return r.choice(options)


def quote(r, value):
    return f'"{value}"' if r.random() < 0.25 else value


def line(r, fields):
    return ",".join(quote(r, f) for f in fields)


def malformed(r, fields):
    text = line(r, fields)
    kind = r.randrange(6)
    if kind == 0:
        return text + ",x"
    if kind == 1:
        return text.rsplit(",", 1)[0]
    if kind == 2:
        return '"' + text
    if kind == 3:
        return text + '"'
    if kind == 4:
        return ""
    return text.replace(",", ',"', 1)


def write_files(r, folder, header, lines):
    folder.mkdir(parents=True, exist_ok=True)
    count = r.randint(1, 4)
    buckets = [[] for _ in range(count)]
    for ln in lines:
        buckets[r.randrange(count)].append(ln)
    for i, b in enumerate(buckets):
        path = folder / f"part-{i:05d}.csv"
        if not b and r.random() < 0.3:
            path.write_bytes(b"")  # zero bytes, not even a header
            continue
        text = header + "\n" + "\n".join(b)
        if b and r.random() < 0.8:
            text += "\n"
        path.write_bytes(text.encode())


def make_case(r, root):
    start_day = r.randrange(20089, 21550)  # 2025-01-01 .. 2028-12-31
    if r.random() < 0.2:
        start_day = r.choice([20513, 20514, 21000, 21243, 21244, 21183, 21184, 20819, 20454])
    window_start = start_day * DAY + (0 if r.random() < 0.8 else r.randrange(DAY))
    window_end = window_start + r.choice([1, 2, 3, 7, 14, 14, 14, 20]) * DAY + (0 if r.random() < 0.8 else r.randrange(DAY))
    (root / "manifest.json").write_text(
        f'{{"window_start": "{f1(window_start - window_start % 1000)}", "window_end": "{f1(window_end - window_end % 1000)}"}}')
    window_start -= window_start % 1000
    window_end -= window_end % 1000

    categories = r.sample(["shoes", "books", "unknowm", "unknownz", "Zeta", "apple", "toys", "a b"], 4)
    products = [f"p{i}" for i in range(1, 9)]
    with open(root / "products.csv", "w", newline="") as f:
        f.write("product_id,category\n")
        for p in products:
            f.write(f"{p},{r.choice(categories)}\n")
    campaigns = [f"c_{i}" for i in range(1, 6)] + ["C_1", "c_x9"]
    with open(root / "campaigns.csv", "w", newline="") as f:
        f.write("campaign_id,channel\n")
        for c in campaigns:
            f.write(f"{c},{r.choice(['search', 'social', 'email'])}\n")

    users = ["u1", "u2", "u3", "u4", "u5", "u6", "u7", "u8", "U1", "u1 ", " u1", "A\tB",
             "a_very_long_user_identifier_42", "u12345678901", "u123456789", "ü1", "u9\r", "zz"]
    users = r.sample(users, r.randint(3, len(users)))

    links = []
    for _ in range(r.randint(0, 25)):
        a, b = r.choice(users + ["x1", "x2", "B0"]), r.choice(users + ["x1", "x2"])
        if r.random() < 0.05:
            b = a
        if r.random() < 0.05:
            a = ""
        links.append(malformed(r, [a, b]) if r.random() < 0.05 else line(r, [a, b]))
    if r.random() < 0.2:
        links.append("user_id_a,user_id_b")
    write_files(r, root / "links", "user_id_a,user_id_b", links)

    # anchors: session-ish times per user
    def some_time():
        if r.random() < 0.15:
            return r.choice([window_start, window_start - 1, window_end - 1, window_end, window_start - 1000,
                             window_end - 1000])
        return r.randrange(window_start - DAY // 2, window_end + DAY // 2)

    anchors = {u: [some_time() for _ in range(r.randint(1, 3))] for u in users}
    events = []
    times = []
    n = r.randint(5, 120)
    for i in range(n):
        user = r.choice(users)
        ts = r.choice(anchors[user]) + r.choice([0, 0, 1, 999, 1000, 60_000, 1_799_999, 1_800_000, 1_800_001,
                                                   3_600_000, r.randrange(0, 4 * 3_600_000)])
        if r.random() < 0.3:
            ts = ts - ts % 1000
        times.append(ts)
        anchors[user].append(ts)
        ingest = ts - ts % 1000 + r.choice([0, 1000, 60_000, DAY, DAY + 1000, -3000, 2 * DAY, 600_000])
        kind = r.choices(["view", "cart", "purchase", "Purchase", "cart ", "", "click"], [40, 20, 30, 2, 2, 2, 2])[0]
        product = r.choice(products + ["p99", "P1", "", " p1"])
        price = r.choice(["100", "0250", "1", "10000000", "10000001", "0", "00", "19.99", "-5", "+500", "1e3", "",
                          "000000000000000000700", "2\r", "２５０", "500", "777"])
        ts_text = good_ts(r, ts) if r.random() < 0.9 else bad_ts(r, ts)
        event_id = f"e{i}" if r.random() < 0.97 else r.choice(["", "e 1", "E0", "e0\r", "an_event_id_that_is_long"])
        fields = [event_id, user if r.random() < 0.97 else "", ts_text, f1(ingest), kind, product, price]
        if event_id in ("e 1", "e0\r", "an_event_id_that_is_long") and any(e[0][0] == event_id for e in events):
            event_id = f"e{i}"
            fields[0] = event_id
        events.append((fields, ingest))
        # duplicates: another ingest_ts, possibly changing anything; a copy at an
        # ingest_ts already used by this event must be identical to that copy (rules.md)
        used = {ingest}
        for _ in range(r.choice([0, 0, 0, 0, 1, 1, 2, 3])):
            copy = list(fields)
            if r.random() < 0.3:
                events.append((copy, ingest))
                continue
            delta = r.choice([1000, 2000, 60_000, 600_000, -1000, -60_000])
            if ingest + delta in used:
                continue
            used.add(ingest + delta)
            copy[3] = f1(ingest + delta)
            if r.random() < 0.4:
                copy[1] = r.choice(users)
            if r.random() < 0.3:
                copy[2] = good_ts(r, ts) if r.random() < 0.5 else bad_ts(r, ts)
            if r.random() < 0.2:
                copy[4] = r.choice(["view", "purchase", "Purchase"])
            events.append((copy, ingest + delta))
    ev_lines = []
    for fields, _ in events:
        ev_lines.append(malformed(r, fields) if r.random() < 0.03 else line(r, fields))
    if r.random() < 0.2:
        ev_lines.append("event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents")
    r.shuffle(ev_lines)
    write_files(r, root / "events", "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents", ev_lines)

    clicks = []
    for i in range(r.randint(0, 60)):
        user = r.choice(users)
        base = r.choice(anchors[user])
        ts = base - r.choice([0, 1, -1, WEEK, WEEK + 1, WEEK - 1, 3_600_000, r.randrange(0, 9 * DAY)])
        click_id = r.choice([f"k{i}", f"k{i}", f"k{i}", f"K{i}", f"k {i}", f"click_long_identifier_{i}"])
        campaign = r.choice(campaigns + ["c_99", "c_1 ", ""])
        ts_text = good_ts(r, ts) if r.random() < 0.92 else bad_ts(r, ts)
        fields = [click_id if r.random() < 0.98 else "", user if r.random() < 0.97 else "", campaign, ts_text]
        clicks.append(line(r, fields))
        if r.random() < 0.1:  # same millisecond, another click
            clicks.append(line(r, [f"k{i}b", user, r.choice(campaigns), ts_text]))
        if r.random() < 0.1:  # an identical copy, quoted differently
            clicks.append(line(r, fields))
        if r.random() < 0.03:
            clicks.append(malformed(r, fields))
    # click ids must be unique across distinct clicks: drop clashes
    seen = {}
    unique = []
    for ln in clicks:
        parsed = reference.parse_line(ln.encode(), 4)
        key = parsed[0] if parsed else None
        if key is not None and key in seen and seen[key] != parsed:
            continue
        if key is not None:
            seen[key] = parsed
        unique.append(ln)
    r.shuffle(unique)
    write_files(r, root / "clicks", "click_id,user_id,campaign_id,ts", unique)


def table_rows(folder, table):
    df = load_actual(Path(folder) / table, SCHEMAS[table])
    return sorted(df.rows(), key=lambda row: tuple("" if v is None else str(v) for v in row))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("binary")
    ap.add_argument("--cases", type=int, default=300)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--keep", default=None, help="copy failing cases here")
    args = ap.parse_args()
    failures = 0
    for case in range(args.cases):
        r = random.Random(args.seed * 1_000_003 + case)
        with tempfile.TemporaryDirectory(ignore_cleanup_errors=True) as tmp:
            tmp = Path(tmp)
            data, ref_out, out = tmp / "data", tmp / "ref", tmp / "out"
            data.mkdir()
            make_case(r, data)
            reference.run(data, ref_out)
            env = dict(os.environ, NCPU=str(r.randint(1, 8)), SS_CHUNK=str(r.choice([1, 2, 7, 64, 200, 1 << 20])),
                       SS_PARTS=str(r.choice([1, 2, 3, 7])))
            p = subprocess.run([args.binary, str(data), str(out)], env=env, capture_output=True, text=True)
            ok = p.returncode == 0
            detail = p.stderr[-1500:] if not ok else ""
            if ok:
                for table in SCHEMAS:
                    a, b = table_rows(ref_out, table), table_rows(out, table)
                    if a != b:
                        ok = False
                        sa, sb = set(a), set(b)
                        detail += f"{table}: only reference {sorted(sa - sb, key=str)[:5]}\n" \
                                  f"{table}: only pipeline  {sorted(sb - sa, key=str)[:5]}\n"
            if not ok:
                failures += 1
                print(f"FAIL case {case} (NCPU={env['NCPU']} SS_CHUNK={env['SS_CHUNK']} SS_PARTS={env['SS_PARTS']})\n{detail}")
                if args.keep:
                    dest = Path(args.keep) / f"case-{args.seed}-{case}"
                    shutil.rmtree(dest, ignore_errors=True)
                    shutil.copytree(data, dest)
    print(f"{args.cases - failures}/{args.cases} fuzz cases match")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
