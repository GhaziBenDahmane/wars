# agentwars

Racer for the SuperChallenge "Agents War" (200 questions, 1,000,000 points).
Leaderboard ties are broken by elapsed time, so the race loop has no pause.

- Every question family seen so far is solved exactly in microseconds
  (`src/solvers.rs`). `tests/corpus.rs` replays every verified prompt from real
  races (`data/agentwars_prompts.jsonl`).
- Each answer goes out on one of two warm HTTP/2 connections. If no response
  arrives within `QUIZ_SC_HEDGE_MS`, a duplicate goes out on the other
  connection. The server dedupes repeats (`isReplay`), so the 2 s deadline
  survives a stalled request.
- Chromium only fetches the Turnstile token that `startRunV2` requires. It is
  killed before the race starts, and logs are written once the race is over.

## Control panel

`agentwars serve` (the container default) serves a page with three buttons:

- **Bench** measures API round-trip times.
- **Dry run** gets a Turnstile token and warms the connections, without
  starting a run.
- **Race** runs the race and **spends one attempt**. It asks for confirmation
  first.

Every button needs the `QUIZ_SC_RUN_TOKEN` value, typed into the page. Only one
job runs at a time, and its output streams into the page. Each job runs on its
own thread, so serving the page never slows the race.

All configuration lives in environment variables (see `.env.example`). The
same jobs also exist on the command line: `agentwars race`, `dry-run`,
`bench [--rounds N]`, and `solve "<prompt>"`.

## Dokploy

1. Create an **Application** from this repository with build type **Dockerfile**.
2. Under **Environment**, set `QUIZ_SC_RUN_TOKEN`, `QUIZ_SC_EMAIL`, and
   `QUIZ_SC_NICKNAME`, plus any optional variables.
3. Under **Domains**, add a domain on container port **3000**.
4. Under **Mounts**, add a volume at `/data`. Race logs go to `/data/runs`.
5. Deploy, open the domain, enter the run token, then press **Dry run**, and
   **Race** once the dry run succeeds.

The API runs on Vercel in Frankfurt (`fra1`). A server in or near Frankfurt
gives the lowest round trip. Check it with **Bench**.

## Development

```
cargo test
cargo build --release
```
