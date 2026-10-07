#!/usr/bin/env python3
"""Compare the race settings that `serve` takes turns between.

Each race is reduced to the median server time (to response headers, so
pauses do not count) of answers 10-29: every race has them, aborted ones
included, and the first answers of a run are slower whatever the setting.
Each race is compared with the races just before and after it that used
another value of the setting, which ran in the same server conditions; the
median difference comes with a 95% bootstrap interval, so a difference
inside the interval is noise. Races that ran to the end are listed apart:
their total is what the leaderboard ranks.

    python3 experiment.py RUNS_DIR [RUNS_DIR...] [--since UNIX_TS]
"""
import collections
import glob
import json
import os
import random
import statistics
import sys

ANSWERS = slice(10, 30)
IGNORED = {"full_race", "engine"}


def label(value):
    return "+".join(map(str, value)) if isinstance(value, list) else str(value)


def races(dirs, since):
    for path in (p for d in dirs for p in glob.glob(os.path.join(d, "race-*.jsonl"))):
        if os.path.getmtime(path) < since:
            continue
        try:
            lines = [json.loads(line) for line in open(path)]
        except (OSError, ValueError):
            continue
        start = next((x for x in lines if x.get("event") == "start"), None)
        variant = (start or {}).get("variant")
        times = [x["headers_ms"] for x in lines if x.get("event") == "answer" and "headers_ms" in x]
        if start is None or not isinstance(variant, dict) or len(times) < ANSWERS.stop:
            continue
        score = next((x for x in lines if x.get("event") == "score"), None)
        yield {
            "t": start["t"],
            "median": statistics.median(times[ANSWERS]),
            "full": len(times) >= 200,
            "elapsed": score["response"].get("agentWars", {}).get("elapsedMs") if score else None,
            "settings": {k: label(v) for k, v in variant.items() if k not in IGNORED},
        }


def interval(values, rounds=2000):
    """95% bootstrap interval of the median."""
    if len(values) < 5:
        return float("nan"), float("nan")
    rng = random.Random(0)
    medians = sorted(statistics.median(rng.choices(values, k=len(values))) for _ in range(rounds))
    return medians[int(rounds * 0.025)], medians[int(rounds * 0.975)]


def main():
    args = sys.argv[1:]
    since = 0.0
    if "--since" in args:
        index = args.index("--since")
        since = float(args[index + 1])
        del args[index:index + 2]
    rows = sorted(races(args or [os.path.expanduser("~/runs")], since), key=lambda r: r["t"])
    print(f"{len(rows)} races; median server ms of answers 10-29, against neighbouring races")
    varying = sorted({k for r in rows for k in r["settings"]
                      if len({x["settings"].get(k) for x in rows}) > 1})
    for name in varying:
        print(f"\n{name}:")
        groups = collections.defaultdict(list)
        deltas = collections.defaultdict(list)
        for i, row in enumerate(rows):
            value = row["settings"].get(name)
            groups[value].append(row)
            others = [rows[j]["median"] for j in (i - 2, i - 1, i + 1, i + 2)
                      if 0 <= j < len(rows) and rows[j]["settings"].get(name) != value]
            if others:
                deltas[value].append(row["median"] - statistics.mean(others))
        for value, group in sorted(groups.items(), key=lambda kv: str(kv[0])):
            d = deltas[value]
            low, high = interval(d)
            delta = statistics.median(d) if d else float("nan")
            full = sorted(r["elapsed"] for r in group if r["elapsed"])
            print(f"  {str(value):30} races {len(group):4}  median {statistics.median(r['median'] for r in group):5.1f}"
                  f"  vs neighbours {delta:+5.2f} ms [{low:+.2f}, {high:+.2f}]"
                  f"  full races {len(full):3} best {full[0] if full else '-'}")
    totals = sorted(r["elapsed"] for r in rows if r["full"] and r["elapsed"])
    if totals:
        print(f"\nfull races: {len(totals)}, best {totals[:5]}, median {statistics.median(totals)}")


if __name__ == "__main__":
    main()
