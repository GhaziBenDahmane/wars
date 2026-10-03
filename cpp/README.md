# agentwars (C++)

A C++20 rewrite of the racer's hot path: `race`, `dry-run`, `bench`,
`cookie-bench` and `solve`, with the same flags, environment variables
(`QUIZ_SC_*`, `QUIZ_LLM_*`), defaults and run-log format as the Rust binary.
`serve`, `team` and the web page stay in Rust.

```sh
make            # build/agentwars: OpenSSL and libstdc++ static, needs glibc >= 2.34
make test       # unit tests, mock-server races, the whole corpus
make difftest   # every corpus prompt + random variants, compared with the Rust solver
```

## What changed

- **Solver**: hand-written matchers replace the regexes, with the same
  leftmost-first semantics. `make difftest` compared it with the Rust solver
  on 41,000+ prompts (corpus prompts and mutations of them): no difference. A
  corpus prompt takes about 0.6 µs to solve (p99 under 2 µs).
- **HTTP**: a purpose-built HTTP/1.1 client on OpenSSL instead of reqwest.
  Keep-alive connection pools, request bytes written before `start()`
  returns, and one `poll()` reactor for hedging. A hedge loser keeps running
  in the background and returns its connection to the pool instead of
  dropping a warm connection. TLS sessions are resumed on new connections.
- **Submissions**: the request body is pasted together from a prefix built
  once per run. Per-answer log records only become JSON after the run.
- **Chrome**: a minimal CDP WebSocket client (`browser.cpp`). Chrome runs in
  its own process group and is killed as a group.

Differences from the Rust binary:

- HTTP/1.1 only (`QUIZ_SC_HTTP1` has no effect). Each route is its own
  connection, so hedging behaves as before.
- No proxy support (Rust's API client already bypassed proxies; the LLM
  client did not).
- `bench` times the fresh-connection probes itself (DNS, TCP, TLS, TTFB)
  instead of running curl.

## Deploy to Frankfurt

Same steps as for the Rust binary (see `../AGENTS.md`), with the C++ binary:

```sh
make test && make
scp -i ~/.ssh/hi.pem build/agentwars ubuntu@18.193.103.147:/tmp/agentwars-cpp
ssh -i ~/.ssh/hi.pem ubuntu@18.193.103.147 '. ~/agentwars.env && /tmp/agentwars-cpp bench --rounds 30'
```
