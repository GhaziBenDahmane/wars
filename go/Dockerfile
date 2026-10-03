FROM golang:1.26-bookworm AS build
WORKDIR /src
COPY . .
RUN go vet ./... && go test ./... && CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /agentwars .

FROM debian:bookworm-slim
# Chromium is only used for a few seconds before the race, for the Turnstile token.
RUN apt-get update \
 && apt-get install -y --no-install-recommends chromium ca-certificates fonts-liberation curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /agentwars /usr/local/bin/agentwars
ENV QUIZ_SC_CHROME=chromium \
    QUIZ_SC_RUNS_DIR=/data/runs
VOLUME /data
WORKDIR /data
EXPOSE 3000
# Races non-stop (one new email per race) and serves a page that streams it.
CMD ["agentwars", "serve"]
