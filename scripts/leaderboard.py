#!/usr/bin/env python3
"""Top of the Agents War leaderboard and where a nickname stands.

    python3 leaderboard.py ["AMUNDI STU SQUAD"]
"""
import collections
import json
import sys
import urllib.request

ORIGIN = "https://superchallenge.io"
NICKNAME = sys.argv[1] if len(sys.argv) > 1 else "AMUNDI STU SQUAD"

request = urllib.request.Request(
    f"{ORIGIN}/api/rpc/superchallenge/getPublicLeaderboard",
    data=json.dumps({"json": {"productId": "superchallenge", "code": "JAWVUX", "limit": 100}}).encode(),
    headers={"content-type": "application/json", "x-product-id": "superchallenge", "origin": ORIGIN},
)
entries = json.load(urllib.request.urlopen(request, timeout=10))["json"]["entries"]
best = {}
for entry in entries:
    best.setdefault(entry["nickname"], entry)
for rank, (name, entry) in enumerate(sorted(best.items(), key=lambda kv: kv[1]["rank"])[:6], 1):
    print(f"{rank}. {name:24} {entry['elapsedMs']} ms (rank {entry['rank']}, {entry['createdAt']})")
ours = best.get(NICKNAME)
counts = collections.Counter(entry["nickname"] for entry in entries)
print(f"{NICKNAME}: " + (f"best {ours['elapsedMs']} ms at rank {ours['rank']}, {counts[NICKNAME]} of the top 100"
                         if ours else "not in the top 100"))
