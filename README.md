# agentwars

Racer for the SuperChallenge "Agents War" (200 questions, 1,000,000 points).
Leaderboard ties are broken by elapsed time, so the race loop has no pause.

- Every question family seen so far is solved exactly in microseconds
  (`src/solvers.rs`). `tests/corpus.rs` replays every verified prompt from real
  races (`data/agentwars_prompts.jsonl`). Representative prompts exercise every
  solver family before the race to move first-use initialization off the clock.
- Before racing, eight independent clients are warmed and compared using
  `getCompetition`. The best two are rechecked, and their existing connection
  pools become the primary and backup. If no response arrives within
  `QUIZ_SC_HEDGE_MS` (150 ms by default), a duplicate goes out on the backup.
  The server dedupes repeats (`isReplay`); the existing request cap remains four.
- Chromium only fetches the Turnstile token that `startRunV2` requires. It is
  killed before the race starts, and logs are written once the race is over.
- Each answer log separates time to response headers from body-read time and
  records the winning route, peer, HTTP version, `x-vercel-id`, and
  `Server-Timing` when supplied. This instrumentation stays in memory until the
  run ends.

## Non-stop racing

`agentwars serve` (the container default) races without stopping and **spends
attempts until it is stopped**. It runs one race at a time, taking turns
between the comma-separated `QUIZ_SC_EMAIL_PREFIXES` (default `ghazi_`). Each
prefix generates `{prefix}{i}@amundi.com` emails,
with the leaderboard name `AMUNDI STU SQUAD`. Each email races
`QUIZ_SC_ATTEMPTS_PER_EMAIL` times (default 10), then the next email starts.
After a failed race it waits 10 s.

The next race (`index attempt`) is written to
`QUIZ_SC_RUNS_DIR/next_race_{prefix}` (e.g. `next_race_ghazi` for `ghazi_`) before each
race, so a restart resumes where it stopped and never repeats an
attempt. `QUIZ_SC_EMAIL_START` (default 1) sets the first index; it only wins
when it is higher than the saved one. With `QUIZ_SC_ALTERNATE_HTTP=1`,
every other race uses HTTP/1.1 instead of HTTP/2; the log names the protocol
of each race.

The page is public and read-only: it shows the email and attempt in progress
and streams its live log. The race loop runs on its own thread, so serving the
page never slows the race.

For a standalone VM, bind the page to loopback and reach it through SSH:

```
agentwars serve --host 127.0.0.1 --port 3000
ssh -L 3000:127.0.0.1:3000 ubuntu@SERVER_IP
```

All configuration lives in environment variables (see `.env.example`). The
same jobs also exist on the command line: `agentwars race`, `dry-run`,
`bench [--rounds N]`, and `solve "<prompt>"`.

## Comparing hosts

```
agentwars bench --network-probes 3 --rounds 20
```

The benchmark separates two different measurements:

- **Network setup:** three sequential, fresh curl connections by default,
  reporting DNS resolution, TCP connection establishment, TLS handshake,
  combined setup, time to first byte (TTFB), post-setup wait, and total time.
  TCP is `time_connect - time_namelookup`; TLS is
  `time_appconnect - time_connect`, not the cumulative curl values. The TCP
  summary excludes DNS and TLS. DNS caches may still be warm across probes.
- **Endpoint latency:** warm `getCompetition` timings from the actual reqwest
  clients. These include that endpoint's server work and must not be treated
  as network-only latency or as a prediction of `submitAnswerV2` latency.

Both curl probes and reqwest warmups print the peer address, negotiated HTTP
version, full `x-vercel-id`, and `Server-Timing` when available. For the observed
Frankfurt deployment, a routing prefix such as `fra1::fra1::...` is useful
context, not a latency guarantee or proof of a particular backend instance.
Missing routing headers are reported as `None`, not guessed. TTFB and post-setup
wait still contain network and server work; neither is pure server processing.

The curl probes are separate from the race's connection pools, use the same
read-only `getCompetition` operation, and never start a race. They disable
curl configuration files and explicit proxies, keep certificate verification
enabled, and stop on any failed HTTP response (including 429). No response body
or cookies are logged. curl's TLS/HTTP implementation may differ from reqwest,
so compare the printed protocol and routing metadata as well as timings.

Network diagnostics require **curl 7.83 or newer**, included in the Docker
runtime. On an existing Ubuntu host, install/update it with
`sudo apt-get install curl` if needed. Set `QUIZ_SC_NETWORK_PROBES` or
`--network-probes` to 0-10 (default 3); zero skips curl and retains the endpoint
benchmark. Neither `race` nor `dry-run` launches curl.

## Race preparation

`race` and `dry-run` prewarm representative solver prompts, create exactly two
independent HTTP/2 clients, and make one read-only `getCompetition` warmup on
each. They do not rank connections by that endpoint: measurements showed its
server-time variance does not predict `submitAnswerV2`, and intensive probing
immediately before a race increased submission latency. `QUIZ_SC_CONNECTIONS`
is therefore bench-only; races always retain one primary and one hedge route.

Existing environment values override the defaults. Keep `QUIZ_SC_HEDGE_MS=150`
as required by the deployment notes: lower delays have caused excessive
duplicates and 429s. The threshold remains configurable with `--hedge-ms` or
the environment. `QUIZ_SC_MAX_REQUESTS` is unchanged and still counts both
speculative duplicates and retries after failures.

## Team mode

`agentwars team --csv team.csv` takes the next teammate in the file every hour
and races their 10 attempts back to back. Over a day the turns cover every
hour, including the quiet ones. The CSV has one `nickname,email` per line; a
header, blank lines and `#` comments are skipped:

```csv
nickname,email
Ada,ada@example.com
Bob,bob@example.com
```

Each turn spends `--attempts` (`QUIZ_SC_TEAM_ATTEMPTS`, 1-10, default 10)
attempts. A failed attempt is logged and the turn goes on; the loop runs until
it is stopped. `--every-min` (`QUIZ_SC_TEAM_EVERY_MIN`, default 60) sets the
interval. `--start N` resumes the rotation at turn `N` after a restart. On the
Frankfurt instance, run it detached:

```sh
. ~/agentwars.env && nohup agentwars team --csv ~/team.csv >> ~/runs/team.log 2>&1 &
```

## Dokploy

1. Create an **Application** from this repository with build type **Dockerfile**.
2. Under **Environment**, set the `QUIZ_LLM_*` variables and any optional
   ones. Email and nickname are generated by `serve`.
3. Under **Domains**, add a domain on container port **3000**.
4. Under **Mounts**, add a volume at `/data`. Race logs go to `/data/runs`.
5. Deploy, open the domain, enter the run token, then press **Dry run**, and
   **Race** once the dry run succeeds.

The API has been observed on Vercel in Frankfurt (`fra1`). Compare hosts using
the **Bench** TCP setup measurements and routing headers, not endpoint RTT alone.

## Development

```
cargo test
cargo build --release
```

The optional curl integration test uses only a local HTTP server:

```
cargo test network::tests::curl_probe -- --ignored
```
