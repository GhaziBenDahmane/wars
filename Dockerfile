FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY tests tests
COPY data data
RUN cargo test --release && cargo build --release

# The Go rewrite, raced in turn by `serve`.
FROM golang:1-bookworm AS build-go
WORKDIR /src/go
COPY go ./
RUN go test ./... && CGO_ENABLED=0 go build -trimpath -ldflags='-s -w' -o /agentwars-go .

# The C++ rewrite (HTTP/1.1 only), raced in turn by `serve`.
FROM debian:bookworm AS build-cpp
RUN apt-get update \
 && apt-get install -y --no-install-recommends g++ make libssl-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY data data
COPY cpp cpp
RUN cd cpp && make clean && make test && make

FROM debian:bookworm-slim
# Chromium is only used for a few seconds before the race, for the Turnstile token.
RUN apt-get update \
 && apt-get install -y --no-install-recommends chromium ca-certificates fonts-liberation curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/agentwars /usr/local/bin/agentwars
COPY --from=build-go /agentwars-go /usr/local/bin/agentwars-go
COPY --from=build-cpp /src/cpp/build/agentwars /usr/local/bin/agentwars-cpp
ENV QUIZ_SC_CHROME=chromium \
    QUIZ_SC_RUNS_DIR=/data/runs \
    QUIZ_SC_ENGINES=rust,go=/usr/local/bin/agentwars-go,cpp=/usr/local/bin/agentwars-cpp:http1 \
    QUIZ_SC_HEDGE_MS_LIST=150,100,80 \
    QUIZ_SC_EDGE_IPS=64.29.17.1,216.150.16.193,216.198.79.1 \
    QUIZ_SC_ABORT_AFTER=30 \
    QUIZ_SC_ABORT_MS=1500 \
    QUIZ_SC_HEADERS_LIST=chrome,bare
VOLUME /data
WORKDIR /data
EXPOSE 3000
# Races non-stop, taking turns between the racers, and serves a page that streams it.
CMD ["agentwars", "serve"]
