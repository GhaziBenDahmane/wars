#!/usr/bin/env python3
"""Pick the start phase and reserve from real answer times.

Replays the racer's duplicate rule (a pair only if every answer left still
fits alone under the run's sliding-window limit, less `reserve`) over answer times drawn from
our own races: paired and single answers apart, answers 0-9 apart (a run's
first answers are slower). By default only races in the fastest tenth
(answers 10-29) are used, since only fast periods can give a best time.

    python3 simulate.py RUNS_DIR [--since UNIX_TS] [--all]
"""
import glob
import json
import os
import random
import statistics
import sys

LIMIT, WINDOW = 250, 10.0
PHASES = (5.4, 5.7, 6.0, 6.3, 6.6)
RESERVES = (0, 3, 5, 10)


def samples(runs, since, fastest):
    races = []
    for path in glob.glob(os.path.join(runs, "race-*.jsonl")):
        if os.path.getmtime(path) < since:
            continue
        try:
            answers = [x for x in map(json.loads, open(path)) if x.get("event") == "answer" and "headers_ms" in x]
        except (OSError, ValueError):
            continue
        if len(answers) >= 30:
            races.append((statistics.median(x["headers_ms"] for x in answers[10:30]), answers))
    races.sort(key=lambda r: r[0])
    if fastest:
        races = races[: max(1, len(races) // 10)]
    pools = {(early, pair): [] for early in (True, False) for pair in (True, False)}
    for _, answers in races:
        for i, x in enumerate(answers):
            pools[(i < 10, x["requests"] >= 2)].append(x["headers_ms"])
    return len(races), pools


PACE = 0.035  # the racer's PLAN_PACE


def estimate(counts, at, more=0):
    w = int(at // WINDOW)
    return counts.get(w, 0) + more + counts.get(w - 1, 0) * (1 - (at % WINDOW) / WINDOW)


def pair_fits(counts, at, left, reserve):
    """The racer's rule: a pair now, then every answer left alone at PACE, under the limit."""
    plan = dict(counts)
    ceiling = LIMIT - reserve
    plan[int(at // WINDOW)] = plan.get(int(at // WINDOW), 0) + 2
    if estimate(plan, at) > ceiling:
        return False
    for k in range(1, left + 1):
        then = at + PACE * k
        plan[int(then // WINDOW)] = plan.get(int(then // WINDOW), 0) + 1
        if estimate(plan, then) > ceiling:
            return False
    return True


def race(pools, phase, reserve, rng, greedy=False):
    t, counts, waited, pairs = phase, {}, 0.0, 0

    def estimate_now(at, more=0):
        return estimate(counts, at, more)

    for i in range(200):
        pair = estimate_now(t, 2 + reserve) <= LIMIT if greedy else pair_fits(counts, t, 199 - i, reserve)
        while not pair and estimate_now(t, 1) > LIMIT:
            t += 0.005
            waited += 0.005
        w = int(t // WINDOW)
        counts[w] = counts.get(w, 0) + (2 if pair else 1)
        pairs += pair
        pool = pools[(i < 10, pair)] or pools[(False, pair)]
        t += rng.choice(pool) / 1000
    return t - phase, pairs, waited


def main():
    args = sys.argv[1:]
    since = float(args[args.index("--since") + 1]) if "--since" in args else 0.0
    runs = os.path.expanduser(next((a for a in args if not a.startswith("--") and not a.replace(".", "").isdigit()), "~/runs"))
    count, pools = samples(runs, since, "--all" not in args)
    print(f"answer times from {count} races: " + ", ".join(
        f"{'first 10' if early else 'later'} {'pairs' if pair else 'singles'} {statistics.median(v):.1f} ms ({len(v)})"
        for (early, pair), v in pools.items() if v))
    if not pools[(False, False)]:
        print("no single answers yet: races so far sent every answer twice; times for singles are unknown")
        pools[(False, False)] = pools[(True, False)] = [x * 1.08 for x in pools[(False, True)]]
    rng = random.Random(0)
    greedy = [race(pools, 6.0, 15, rng, greedy=True) for _ in range(300)]
    print(f"greedy rule at 6.0 s, reserve 15: {statistics.mean(r[0] for r in greedy):.3f}s, "
          f"{statistics.mean(r[1] for r in greedy):.0f} pairs, waits {1000 * statistics.mean(r[2] for r in greedy):.0f} ms")
    print("plan rule:")
    print("phase  " + "  ".join(f"reserve {r:2}             " for r in RESERVES))
    for phase in PHASES:
        cells = []
        for reserve in RESERVES:
            results = [race(pools, phase, reserve, rng) for _ in range(300)]
            cells.append(f"{statistics.mean(r[0] for r in results):6.3f}s p5 {sorted(r[0] for r in results)[15]:6.3f}"
                         f" w{1000 * statistics.mean(r[2] for r in results):4.0f}")
        print(f"{phase:4.1f}   " + "  ".join(cells))


if __name__ == "__main__":
    main()
