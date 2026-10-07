# Findings

What the AMUNDI STU SQUAD learned while taking the Agents War leaderboard
from about 8.2 s to **6,824 ms**, mostly from 2026-10-03 to 2026-10-06. Every
number here comes from our own race logs (thousands of races) and from a
monitor that probed the server while we weren't racing.

## Where the time goes

- **The leaderboard time is server time.** It equals our own clock (from the
  `startRunV2` reply to the last answer) within about 5 ms. Solving takes
  microseconds and the client adds about 1 ms per answer. From AWS Frankfurt,
  TCP connect to the Vercel edge takes 0.7-0.9 ms, so the remaining 30-50 ms
  per answer is the server's.
- **Part of that is Vercel itself.** A submitAnswerV2 with an invalid token
  (no run work at all) takes 18-28 ms at best. An empty function we deployed
  in the same region had a median of 28.9 ms, against 59.1 ms for their
  invalid answer at the same moment. About half of an answer's time is the
  platform.
- **The server's speed drifts much more than any client setting.** The
  per-race median answer time ranges from 38 to 70 ms and comes in regimes:
  consecutive races correlate at 0.59, and the correlation is gone after
  about 20 races. Evenings (about 20:00-01:00 UTC) were the fastest. On
  2026-10-05 at 00:00 UTC the server got 7-11 ms slower in one step, even
  with none of our races running.
- **Spikes come from the backend.** When one racer saw an answer over 70 ms,
  another racer's overlapping answer was slow too in 64% of cases (18% at
  baseline). A backup copy can't dodge a spike that hits every request.
- **Warm nodes are faster.** An answer handled by the same Vercel node as the
  previous one took 42.4 ms (median). A node new to the run took 49.7 ms. A
  connection doesn't keep its node, though (every connection's favourite node
  changed within 20 s), so this can't be pinned.

Since the server sets the pace, the strategy was: never add to the critical
path, save the few milliseconds the protocol allows, and race as often as
possible to catch the fast minutes.

## What made races faster

| Change | Effect |
|---|---|
| Exact solvers instead of an LLM | The first scouting runs used an LLM at 1-2.5 s per answer. Exact solvers take microseconds and are never wrong on known families |
| Racing from AWS `eu-central-1`, next to Vercel `fra1` | About 10 ms less per answer than from OVH Dunkirk, so about 2 s per race |
| Everything off the clock: token, connections, solver warmup | The race loop is one solve and one round trip per question |
| **Answer pairs** (the same answer twice at once) | About 3-5 ms sooner per paired answer: the server answers whichever copy it finishes first |
| **Start 6 s into the rate-limit window** | The race spans two windows, so about 135 of 200 answers go as pairs with no 429 |
| Look-ahead pair rule | 80-100 ms faster per race than the greedy rule, with no waits at the end of the race (simulated over real answer times) |
| Pinning the two connections to two edges (`64.29.17.1` + `216.150.16.193`) | About 0.4 ms faster per answer (95% interval −0.60 to −0.06 ms) |
| Re-warming the connections 300 ms before the start | Idle HTTP/2 connections answer their first request slowly |
| Aborting slow starts | First 30 answers over 1.5 s: 1.7× more top-5% races per hour in simulation, keeping all of the top 20. Later relaxed to 4 s, so almost every race runs to the end and saves its score |
| Persistent Chrome per racer, token fetched during the race | No gap between races. Launching a Chrome for every token on three racers had saturated the CPU |
| Staggered starts (0.3 s apart) | Five racers starting at the same instant slowed every answer by about 3 ms |

The pairing trick wasn't ours alone. The leading rival's leaderboard
timestamps clustered 3.6-4.6 s into the 10 s windows, which means a start
about 6.3 s in: the same idea. After the pairs went live, the remaining gap
was racing in the right minutes.

## What didn't help

Each was tested side by side, races taking turns, with no difference outside
the noise:

- HTTP/1.1 vs HTTP/2
- the Rust racer vs the Go and C++ rewrites (both were deleted afterwards)
- locale `en` vs `fr`
- a fresh email, a fresh IP, a different instance
- answer headers: Chrome's user agent and cookie, none, or only `content-type`
- the real play page vs the stub page for the Turnstile token
- hedging on or off, and a backup copy after 70 ms instead of 150 ms (spikes
  hit the backup copy too)
- following the connection that won the last hedge
- a pause of 3 or 8 ms before each answer (it only adds its own time)
- the start phase between 5.4 s and 6.6 s (all within about 50 ms)
- racing from inside Vercel `fra1`: invalid-answer timings from a Vercel
  function had a median of 58.5 ms, against 51.8 ms from EC2, with the same
  minimum of about 26 ms

And a few things that made it worse:

- **A hedge under 150 ms** duplicated most requests and ran into
  `429 TOO_MANY_REQUESTS`.
- **More racers.** Six staggered racers made every answer about 1.8 ms slower
  than two, without more fast races. Our own load cost 7-11 ms on the idle
  probe when all four racers overlapped, and nothing with two or fewer.
- **Waiting for a fast server.** A gate that held races while the server was
  slow let only about 110 races an hour through per racer. A plain 10 s pause
  after each race gave about 170.
- **Answers sent 6-7 s into a window** were about 3 ms slower than those sent
  at 3-4 s (relative to each race's own median), with more spikes.

## How to test a setting

The server drifts by several milliseconds within an hour, so a before/after
comparison mostly measures the drift. What worked:

1. Put the values in a `serve` list (`QUIZ_SC_HEDGE_MS_LIST=80,150`) so races
   take turns between them in the same conditions.
2. Keep the number of racers fixed, since our own load costs 1-3 ms per
   answer.
3. Judge each race on the median of answers 10-29: every race has them,
   aborted ones included, and the first answers of a run are slower whatever
   the setting.
4. Compare each race with its neighbours that used another value, and act
   only when the 95% bootstrap interval excludes zero
   (`scripts/experiment.py`).

## Timeline

| Date (UTC) | Best | What changed |
|---|---|---|
| 2026-10-01 | | First scouting runs with an LLM; exact solvers written from the logged prompts |
| 2026-10-03 | 8.23 s | Racing from Frankfurt; 36 client variants found to be noise |
| 2026-10-04 | 8.07 s | Pairs within the sliding window, start phase, token prefetch, persistent Chrome. The rival is at 7.38 s |
| 2026-10-04 to 10-06 | | Look-ahead pair rule, four staggered racers, pause after each race, monitor and own Vercel probe |
| 2026-10-07 | **6.824 s** | #1, with 73 of the top 100 entries |
