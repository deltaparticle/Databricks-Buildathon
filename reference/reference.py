#!/usr/bin/env python3
"""
Reference implementation of rules.md: literal, single-threaded and slow on purpose.
It exists only to check the fast pipeline. Everything is handled as bytes.

Usage: reference.py [DATA_DIR OUTPUT_DIR]   (defaults: $DATA_DIR, $OUTPUT_DIR)

Digit checks use bytes.isdigit(), which for a bytes object accepts only the ASCII
characters 0-9, and int() is only ever called on bytes that passed that check.
"""
import bisect
import datetime
import json
import os
import sys
from collections import defaultdict
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

DAY = 86_400_000
GAP = 1_800_000
WEEK = 7 * DAY
LATE = DAY
INF = float("inf")


# ---------------------------------------------------------------- CSV dialect

def data_lines(path):
    """Every line after the first (the header). Only \\n ends a line."""
    raw = path.read_bytes()
    lines = raw.split(b"\n")
    if lines and lines[-1] == b"":
        lines.pop()  # the text after a final \n is not a line
    return lines[1:]


def parse_line(line, width):
    """The fields of a well-formed line (None for an empty field), or None if malformed."""
    if line == b"":
        return None
    fields = line.split(b",")
    if len(fields) != width:
        return None
    out = []
    for field in fields:
        if b'"' in field:
            if len(field) >= 2 and field[0] == 0x22 and field[-1] == 0x22 and b'"' not in field[1:-1]:
                field = field[1:-1]
            else:
                return None
        out.append(field if field else None)
    return out


def csv_files(folder):
    folder = Path(folder)
    if not folder.is_dir():
        return []
    return sorted(p for p in folder.iterdir()
                  if p.name.endswith(".csv") and not p.name.startswith(".") and p.is_file())


def rows(folder, width):
    for path in csv_files(folder):
        for line in data_lines(path):
            fields = parse_line(line, width)
            if fields is not None:
                yield fields


# ---------------------------------------------------------------- timestamps

def is_leap(year):
    return year % 4 == 0 and (year % 100 != 0 or year % 400 == 0)


def days_in_month(year, month):
    if month == 2:
        return 29 if is_leap(year) else 28
    return 30 if month in (4, 6, 9, 11) else 31


def days_from_civil(year, month, day):
    year -= month <= 2
    era = year // 400
    yoe = year - era * 400
    doy = (153 * ((month + 9) % 12) + 2) // 5 + day - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def parse_ts(value):
    """Milliseconds since the epoch (UTC) for F1, F2 or F3, else None."""
    if value is None:
        return None
    if value.isdigit():  # F3; Python ints never overflow, the window check rejects huge values
        return int(value)
    if len(value) == 20 and value[19:20] == b"Z":
        offset = 0
    elif len(value) == 25 and value[19:20] in (b"+", b"-") and value[22:23] == b":":
        hours, minutes = value[20:22], value[23:25]
        if not (hours.isdigit() and minutes.isdigit()):
            return None
        hours, minutes = int(hours), int(minutes)
        if hours > 23 or minutes > 59:
            return None
        offset = (hours * 60 + minutes) * 60_000
        if value[19:20] == b"-":
            offset = -offset
    else:
        return None
    if (value[4:5], value[7:8], value[10:11], value[13:14], value[16:17]) != (b"-", b"-", b"T", b":", b":"):
        return None
    parts = (value[0:4], value[5:7], value[8:10], value[11:13], value[14:16], value[17:19])
    if not all(part.isdigit() for part in parts):
        return None
    year, month, day, hour, minute, second = (int(part) for part in parts)
    if not (1 <= month <= 12 and 1 <= day <= days_in_month(year, month)):
        return None
    if hour > 23 or minute > 59 or second > 59:
        return None
    local = ((days_from_civil(year, month, day) * 24 + hour) * 60 + minute) * 60_000 + second * 1000
    return local - offset


# ---------------------------------------------------------------- pipeline

def run(data, out):
    data, out = Path(data), Path(out)
    manifest = json.loads((data / "manifest.json").read_text())
    window_start = parse_ts(manifest["window_start"].encode())
    window_end = parse_ts(manifest["window_end"].encode())

    category = {f[0]: f[1] for f in rows_file(data / "products.csv", 2) if f[0] is not None}
    channel = {f[0]: f[1] for f in rows_file(data / "campaigns.csv", 2) if f[0] is not None}

    # Linked IDs
    parent = {}

    def find(x):
        root = x
        while parent[root] != root:
            root = parent[root]
        while parent[x] != root:
            parent[x], x = root, parent[x]
        return root

    for a, b in rows(data / "links", 2):
        if a is None or b is None:
            continue
        for x in (a, b):
            if x not in parent:
                parent[x] = x
        ra, rb = find(a), find(b)
        if ra != rb:
            parent[ra] = rb
    name = {}
    for x in parent:
        r = find(x)
        if r not in name or x < name[r]:
            name[r] = x

    def person(user):
        return name[find(user)] if user in parent else user

    # Events: parse, dedup on event_id (smallest ingest_ts), then validate the kept copy
    kept = {}
    for fields in rows(data / "events", 7):
        event_id = fields[0]
        if event_id is None:
            continue
        ingest = parse_ts(fields[3])
        if ingest is None:
            ingest = INF
        current = kept.get(event_id)
        if current is None or ingest < current[0]:
            kept[event_id] = (ingest, fields)

    events = defaultdict(list)  # person -> [(ts, is_purchase, price, category)]
    for ingest, (_, user, ts, _, event_type, product, price) in kept.values():
        if user is None:
            continue
        t = parse_ts(ts)
        if t is None or not (window_start <= t < window_end):
            continue
        if event_type not in (b"view", b"cart", b"purchase"):
            continue
        if product is None:
            continue
        cents = 0
        if event_type == b"purchase":
            if price is None or not price.isdigit():
                continue
            cents = int(price)
            if not (1 <= cents <= 10_000_000):
                continue
        if ingest - t > LATE:
            continue
        events[person(user)].append((t, event_type == b"purchase", cents, category.get(product, b"unknown")))

    # Clicks: parse, dedup on click_id (copies are identical: keep the first), validate
    seen = set()
    clicks = defaultdict(list)  # person -> [(ts, click_id, campaign)]
    for click_id, user, campaign, ts in rows(data / "clicks", 4):
        if click_id is None or click_id in seen:
            continue
        seen.add(click_id)
        if user is None:
            continue
        t = parse_ts(ts)
        if t is None or not (window_start - WEEK <= t < window_end):
            continue
        if campaign not in channel:
            continue
        clicks[person(user)].append((t, click_id, campaign))

    user_daily = {}
    campaign_daily = {}

    def campaign_row(campaign, day):
        key = (campaign, day)
        if key not in campaign_daily:
            campaign_daily[key] = [0, 0, 0, 0]  # sessions, purchasing, revenue, assisted
        return campaign_daily[key]

    for who, evs in events.items():
        evs.sort(key=lambda e: e[0])
        sessions = []
        for e in evs:
            if sessions and e[0] - sessions[-1][-1][0] <= GAP:
                sessions[-1].append(e)
            else:
                sessions.append([e])
        cl = sorted(clicks.get(who, []))
        click_ts = [c[0] for c in cl]
        for session in sessions:
            start = session[0][0]
            day = start // DAY
            revenue = sum(e[2] for e in session if e[1])
            purchased = any(e[1] for e in session)

            row = user_daily.setdefault((who, day), [0, 0, 0, defaultdict(int)])
            row[0] += 1
            row[1] += len(session)
            row[2] += revenue
            for e in session:
                if e[1]:
                    row[3][e[3]] += e[2]

            lo = bisect.bisect_left(click_ts, start - WEEK)
            hi = bisect.bisect_right(click_ts, start)
            qualifying = cl[lo:hi]
            if qualifying:
                best = min(qualifying, key=lambda c: (-c[0], c[1]))
                credited = best[2]
                for campaign in {c[2] for c in qualifying}:
                    campaign_row(campaign, day)[3] += 1
            else:
                credited = b"organic"
            r = campaign_row(credited, day)
            r[0] += 1
            r[1] += purchased
            r[2] += revenue

    write(out, user_daily, campaign_daily, channel)
    self_check(user_daily, campaign_daily)


def rows_file(path, width):
    if not path.is_file():
        return []
    return [f for f in (parse_line(line, width) for line in data_lines(path)) if f is not None]


def to_date(day):
    return datetime.date(1970, 1, 1) + datetime.timedelta(days=day)


def write(out, user_daily, campaign_daily, channel):
    users = sorted(user_daily.items())
    u = {
        "user_id": [k[0].decode() for k, _ in users],
        "day": [to_date(k[1]) for k, _ in users],
        "sessions": [v[0] for _, v in users],
        "events": [v[1] for _, v in users],
        "revenue_cents": [v[2] for _, v in users],
        "top_category": [min(v[3].items(), key=lambda kv: (-kv[1], kv[0]))[0].decode() if v[3] else None
                         for _, v in users],
    }
    user_schema = pa.schema([("user_id", pa.string()), ("day", pa.date32()), ("sessions", pa.int64()),
                             ("events", pa.int64()), ("revenue_cents", pa.int64()), ("top_category", pa.string())])
    camps = sorted(campaign_daily.items())
    c = {
        "campaign_id": [k[0].decode() for k, _ in camps],
        "channel": [(b"organic" if k[0] == b"organic" else channel[k[0]]).decode() for k, _ in camps],
        "day": [to_date(k[1]) for k, _ in camps],
        "sessions": [v[0] for _, v in camps],
        "purchasing_sessions": [v[1] for _, v in camps],
        "revenue_cents": [v[2] for _, v in camps],
        "assisted_sessions": [v[3] for _, v in camps],
    }
    camp_schema = pa.schema([("campaign_id", pa.string()), ("channel", pa.string()), ("day", pa.date32()),
                             ("sessions", pa.int64()), ("purchasing_sessions", pa.int64()),
                             ("revenue_cents", pa.int64()), ("assisted_sessions", pa.int64())])
    for table, columns, schema in (("user_daily", u, user_schema), ("campaign_daily", c, camp_schema)):
        folder = out / table
        folder.mkdir(parents=True, exist_ok=True)
        pq.write_table(pa.table(columns, schema=schema), folder / "part-0.parquet")


def self_check(user_daily, campaign_daily):
    user_sessions = sum(v[0] for v in user_daily.values())
    user_revenue = sum(v[2] for v in user_daily.values())
    camp_sessions = sum(v[0] for v in campaign_daily.values())
    camp_revenue = sum(v[2] for v in campaign_daily.values())
    assisted = sum(v[3] for v in campaign_daily.values())
    credited = sum(v[0] for k, v in campaign_daily.items() if k[0] != b"organic")
    assert user_sessions == camp_sessions, (user_sessions, camp_sessions)
    assert user_revenue == camp_revenue, (user_revenue, camp_revenue)
    assert assisted >= credited, (assisted, credited)
    print(f"reference: {len(user_daily)} user_daily rows, {len(campaign_daily)} campaign_daily rows, "
          f"{user_sessions} sessions", file=sys.stderr)


if __name__ == "__main__":
    if len(sys.argv) >= 3:
        data_dir, out_dir = sys.argv[-2], sys.argv[-1]
    else:
        data_dir, out_dir = os.environ.get("DATA_DIR", "/data"), os.environ.get("OUTPUT_DIR", "/output")
    run(data_dir, out_dir)
