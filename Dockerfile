FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY tests tests
COPY data data
RUN cargo test --release && cargo build --release

FROM debian:bookworm-slim
# Chromium is only used for the Turnstile token, fetched while the previous race runs.
RUN apt-get update \
 && apt-get install -y --no-install-recommends chromium ca-certificates fonts-liberation curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/agentwars /usr/local/bin/agentwars
# The Frankfurt race settings (docs/architecture.md): answers twice while the run's
# sliding-window limit allows, a run started 6 s into a 10 s window, aborts
# only very slow starts, and sleeps 10 s after each race.
ENV QUIZ_SC_CHROME=chromium \
    QUIZ_SC_RUNS_DIR=/data/runs \
    QUIZ_SC_HEDGE_MS_LIST=150 \
    QUIZ_SC_EDGE_IPS=64.29.17.1+216.150.16.193 \
    QUIZ_SC_DUPLICATES_LIST=1 \
    QUIZ_SC_WINDOW_LIMIT=250 \
    QUIZ_SC_WINDOW_RESERVE=3 \
    QUIZ_SC_START_PHASE_MS=6000 \
    QUIZ_SC_ABORT_AFTER=30 \
    QUIZ_SC_ABORT_MS=4000 \
    QUIZ_SC_ABORT_TARGET_MS=0 \
    QUIZ_SC_ABORT_FAST_MS=34 \
    QUIZ_SC_FULL_EVERY=10 \
    QUIZ_SC_PAUSE_MS=10000
VOLUME /data
WORKDIR /data
EXPOSE 3000
# Races non-stop and serves a page that streams it.
CMD ["agentwars", "serve"]
