#!/usr/bin/env python3
"""Races from inside Vercel fra1, to compare with the Frankfurt racers.

The race itself runs in the `agentwars-probe` project's `/api/race` function
(the same Rust binary and settings; that project is not in this repository,
so this script only works with your own copy of it). This driver, on Frankfurt 1, gets each
race's Turnstile token from its own Chrome (`agentwars token`), calls the
function, and writes the race log into the runs folder like the other racers,
with `"platform": "vercel"` in the start event's variant. Emails are
`vercel_{i}@amundi.com`, 10 attempts each, the position saved in
`next_race_vercel` before each race (as `serve` does).

    . ~/agentwars.env && QUIZ_SC_CDP_URL=http://127.0.0.1:9303 python3 vercel_racer.py
"""
import json
import os
import subprocess
import sys
import time
import urllib.request

URL = "https://agentwars-probe.vercel.app/api/race"
KEY = open(os.path.expanduser("~/.config/agentwars/racer.key")).read().strip()
RUNS = os.path.expanduser(os.environ.get("QUIZ_SC_RUNS_DIR", "~/runs"))
PREFIX = "vercel_"
ATTEMPTS = 10
START_PHASE_MS = 6600  # the Frankfurt racers start at 5.4, 5.7, 6.0 and 6.3 s
PAUSE = 10  # seconds after each race, as QUIZ_SC_PAUSE_MS on Frankfurt
FAILURE_PAUSE = 10
POSITION = os.path.join(RUNS, "next_race_vercel")


def load_position():
    try:
        index, attempt = open(POSITION).read().split()
        return int(index), int(attempt)
    except (OSError, ValueError):
        return 1, 1


def save_position(index, attempt):
    with open(POSITION, "w") as file:
        file.write(f"{index} {attempt}\n")


def token():
    output = subprocess.run(["/usr/local/bin/agentwars", "token"], capture_output=True, text=True, timeout=90, check=True)
    return json.loads(output.stdout)


def race(email, credentials):
    body = json.dumps({**credentials, "email": email, "start_phase_ms": START_PHASE_MS}).encode()
    request = urllib.request.Request(URL, body, {"content-type": "application/json", "x-racer-key": KEY})
    with urllib.request.urlopen(request, timeout=90) as response:
        return json.loads(response.read())


def save_log(result):
    lines = []
    for line in result.get("log", []):
        event = json.loads(line)
        if event.get("event") == "start":
            event["variant"] = {**(event.get("variant") or {}), "platform": "vercel", "region": result.get("region")}
        lines.append(json.dumps(event))
    if not lines:
        return None
    path = os.path.join(RUNS, f"race-{int(time.time())}-vercel.jsonl")
    with open(path, "w") as file:
        file.write("\n".join(lines) + "\n")
    return path


def main():
    os.makedirs(RUNS, exist_ok=True)
    while True:
        index, attempt = load_position()
        email = f"{PREFIX}{index}@amundi.com"
        try:
            credentials = token()
        except Exception as error:
            print(f"error: token: {error}", flush=True)
            time.sleep(FAILURE_PAUSE)
            continue
        # Saved before racing: a crash mid-race must not repeat this attempt.
        save_position(*((index + 1, 1) if attempt >= ATTEMPTS else (index, attempt + 1)))
        print(f"{email}: attempt {attempt}/{ATTEMPTS} on Vercel", flush=True)
        try:
            result = race(email, credentials)
        except Exception as error:
            print(f"error: calling the function: {error}", flush=True)
            time.sleep(FAILURE_PAUSE)
            continue
        stderr = result.get("stderr", "")
        for line in stderr.splitlines():
            if line.startswith(("run ended", "score saved", "error", "aborted")):
                print(f"  {line[:200]}", flush=True)
        print(f"  log: {save_log(result)} (exit {result.get('code')}, {result.get('region')})", flush=True)
        if result.get("code") != 0:
            if "starting the run" not in stderr:
                save_position(index, attempt)  # no attempt spent: retry it
            print(stderr[-1500:], file=sys.stderr, flush=True)
            time.sleep(FAILURE_PAUSE)
            continue
        time.sleep(PAUSE)


if __name__ == "__main__":
    main()
