FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY tests tests
COPY data data
RUN cargo test --release && cargo build --release

FROM debian:bookworm-slim
# Chromium is only used for a few seconds before the race, for the Turnstile token.
RUN apt-get update \
 && apt-get install -y --no-install-recommends chromium ca-certificates fonts-liberation \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/agentwars /usr/local/bin/agentwars
ENV QUIZ_SC_CHROME=chromium \
    QUIZ_SC_RUNS_DIR=/data/runs
VOLUME /data
WORKDIR /data
EXPOSE 3000
# The control panel. It never races on its own, so a restart cannot spend an attempt.
CMD ["agentwars", "serve"]
