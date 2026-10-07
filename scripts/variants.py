#!/usr/bin/env python3
"""Rank the race variants (hedge, edge, follow) by server-measured elapsed.

Usage: scripts/variants.py RUNS_DIR [--since UNIX_SECONDS]
"""
import collections, glob, json, statistics, sys

runs = sys.argv[1] if len(sys.argv) > 1 else "runs"
since = float(sys.argv[sys.argv.index("--since") + 1]) if "--since" in sys.argv else 0
by = collections.defaultdict(list)
for path in glob.glob(f"{runs}/race-*.jsonl"):
    try:
        lines = [json.loads(line) for line in open(path)]
    except (OSError, ValueError):
        continue
    start = next((l for l in lines if l.get("event") == "start"), None)
    score = next((l for l in lines if l.get("event") == "score"), None)
    if not start or not score or "variant" not in start or start["t"] < since:
        continue
    war = score["response"].get("agentWars", {})
    if war.get("ended") != "goal":
        continue
    v = start["variant"]
    edge = "+".join(v.get("edge_ips") or []) or "dns"
    key = f"{v.get('engine', 'rust'):<4} {'h1' if v.get('http1') else 'h2'} hedge {v['hedge_ms']:>3} edge {edge:<28} {'follow' if v['follow_winner'] else 'fixed'}"
    by[key].append(war["elapsedMs"])

print(f"{'variant':<66} {'n':>4} {'median':>7} {'p10':>6} {'best':>6}")
for key, times in sorted(by.items(), key=lambda kv: statistics.median(kv[1])):
    times.sort()
    print(f"{key:<66} {len(times):>4} {statistics.median(times):>7.0f} "
          f"{times[len(times) // 10]:>6} {times[0]:>6}")
