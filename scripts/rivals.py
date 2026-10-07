#!/usr/bin/env python3
"""Our races against the rivals' leaderboard entries of the same minutes.

For each rival entry recorded by monitor.py, the races we ran within 3
minutes of its start: how many, our best full race, our best and median
server time per answer (answers 10-29, which every race has; the first
answers of a run are slower), and the server probe of those minutes.

    python3 rivals.py RUNS_DIR [MONITOR_DIR] [--since UNIX_TS]
"""
import bisect
import datetime
import glob
import json
import os
import statistics
import sys

US = "AMUNDI STU SQUAD"


def main():
    args = sys.argv[1:]
    since = 0.0
    if "--since" in args:
        index = args.index("--since")
        since = float(args[index + 1])
        del args[index:index + 2]
    runs = os.path.expanduser(args[0] if args else "~/runs")
    monitor = os.path.expanduser(args[1] if len(args) > 1 else "~/monitor")
    rivals = []
    for line in open(os.path.join(monitor, "leaderboard.jsonl")):
        entry = json.loads(line)
        end = datetime.datetime.fromisoformat(entry["createdAt"].replace("Z", "+00:00")).timestamp()
        if entry["nickname"] != US and end > since:
            rivals.append((end - entry["elapsedMs"] / 1000, entry))
    rivals.sort(key=lambda r: r[0])
    if not rivals:
        print("no rival entry recorded yet")
        return
    probes = []
    probe_path = os.path.join(monitor, "probe.jsonl")
    if os.path.exists(probe_path):
        probes = sorted((p["t"], p["ms"]) for p in map(json.loads, open(probe_path)) if p["probe"] == "function")
    probe_times = [t for t, _ in probes]
    ours = []
    low = rivals[0][0] - 300
    for path in glob.glob(os.path.join(runs, "race-*.jsonl")):
        if os.path.getmtime(path) < low:
            continue
        try:
            lines = [json.loads(line) for line in open(path)]
        except (OSError, ValueError):
            continue
        answers = [x["headers_ms"] for x in lines if x.get("event") == "answer" and "headers_ms" in x]
        if len(answers) < 30:
            continue
        score = next((x for x in lines if x.get("event") == "score"), None)
        elapsed = score and score["response"].get("agentWars", {}).get("elapsedMs")
        ours.append((lines[0]["t"], statistics.median(answers[10:30]), elapsed))
    for start, entry in rivals:
        near = [r for r in ours if abs(r[0] - start) <= 180]
        i, j = bisect.bisect_left(probe_times, start - 180), bisect.bisect_right(probe_times, start + 180)
        probe = statistics.median(ms for _, ms in probes[i:j]) if j > i else float("nan")
        when = datetime.datetime.fromtimestamp(start, datetime.UTC).strftime("%m-%d %H:%M:%S")
        line = f"{when} {entry['nickname']:16} {entry['elapsedMs']} ms ({entry['elapsedMs'] / 200:.1f}/answer) | probe {probe:.1f} ms"
        if near:
            full = sorted(r[2] for r in near if r[2])
            line += (f" | ours: {len(near)} races, answers 10-29 best {min(r[1] for r in near):.1f}"
                     f" median {statistics.median(r[1] for r in near):.1f}, best full {full[0] if full else '-'}")
        print(line)


if __name__ == "__main__":
    main()
