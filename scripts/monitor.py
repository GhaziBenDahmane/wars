#!/usr/bin/env python3
"""Records what the races are judged against, while they run.

- Every minute, the leaderboard's top 100: entries not seen before are
  appended to `leaderboard.jsonl` (rivals' entries drop out of the top 100
  later, so this keeps them).
- Every 5 s, the server's speed without our races: `submitAnswerV2` with an
  invalid token (the function's own cost, no run work) and `getCompetition`,
  on one kept-alive connection, appended to `probe.jsonl`. Next to them, our
  own Vercel project in fra1 (`~/agentwars-probe`): a function that answers
  at once (`own_function`, Vercel's own cost) and a static file
  (`own_static`, the edge alone), to tell Vercel's slow hours from theirs.

    python3 monitor.py [DIR]     # default ~/monitor; runs until stopped
"""
import http.client
import json
import os
import sys
import time

HOST = "superchallenge.io"
OWN_HOST = "agentwars-probe.vercel.app"
PATH = "/api/rpc/superchallenge/"
HEADERS = {"content-type": "application/json", "x-product-id": "superchallenge", "origin": f"https://{HOST}"}
BAD_ANSWER = {"productId": "superchallenge", "code": "JAWVUX", "runToken": "x.y", "drillId": "gen-1", "submission": "1"}
COMPETITION = {"productId": "superchallenge", "code": "JAWVUX"}
LEADERBOARD = {"productId": "superchallenge", "code": "JAWVUX", "limit": 100}


class Client:
    def __init__(self, host=HOST):
        self.host = host
        self.connection = None

    def call(self, procedure, body):
        """(milliseconds to the response headers, status, x-vercel-id, body)."""
        return self.request("POST", PATH + procedure, json.dumps({"json": body}), HEADERS)

    def request(self, method, path, body=None, headers={}):
        for attempt in (1, 2):
            try:
                if self.connection is None:
                    self.connection = http.client.HTTPSConnection(self.host, timeout=10)
                    self.connection.connect()
                started = time.perf_counter()
                self.connection.request(method, path, body, headers)
                response = self.connection.getresponse()
                headers_ms = (time.perf_counter() - started) * 1000
                data = response.read()
                return headers_ms, response.status, response.getheader("x-vercel-id"), data
            except (OSError, http.client.HTTPException):
                self.connection = None
                if attempt == 2:
                    raise
        raise AssertionError("unreachable")


def main():
    folder = os.path.expanduser(sys.argv[1] if len(sys.argv) > 1 else "~/monitor")
    os.makedirs(folder, exist_ok=True)
    seen_path = os.path.join(folder, "leaderboard.jsonl")
    seen = set()
    if os.path.exists(seen_path):
        for line in open(seen_path):
            entry = json.loads(line)
            seen.add((entry["nickname"], entry["createdAt"], entry["elapsedMs"]))
    client = Client()
    own = Client(OWN_HOST)
    next_board = 0.0
    while True:
        now = time.time()
        try:
            with open(os.path.join(folder, "probe.jsonl"), "a") as probe:
                for name, procedure, body in (("function", "submitAnswerV2", BAD_ANSWER),
                                              ("competition", "getCompetition", COMPETITION)):
                    ms, status, vercel_id, _ = client.call(procedure, body)
                    probe.write(json.dumps({"t": now, "probe": name, "ms": round(ms, 2), "status": status,
                                            "x_vercel_id": vercel_id}) + "\n")
                for name, path in (("own_function", "/api/ping"), ("own_static", "/static.txt")):
                    try:
                        ms, status, vercel_id, _ = own.request("GET", path)
                    except Exception as error:
                        print(f"monitor: {name}: {error}", file=sys.stderr, flush=True)
                        continue
                    probe.write(json.dumps({"t": now, "probe": name, "ms": round(ms, 2), "status": status,
                                            "x_vercel_id": vercel_id}) + "\n")
            if now >= next_board:
                next_board = now + 60
                _, status, _, data = client.call("getPublicLeaderboard", LEADERBOARD)
                if status == 200:
                    with open(seen_path, "a") as board:
                        for entry in json.loads(data)["json"]["entries"]:
                            key = (entry["nickname"], entry["createdAt"], entry["elapsedMs"])
                            if key not in seen:
                                seen.add(key)
                                board.write(json.dumps({**entry, "seen": now}) + "\n")
        except Exception as error:  # keep recording through outages
            print(f"monitor: {error}", file=sys.stderr, flush=True)
        time.sleep(max(0.0, now + 5 - time.time()))


if __name__ == "__main__":
    main()
