# veille: single static-ish binary, small container (PRP §10).
# Build:  docker build -t veille .
# Run:    docker compose -f compose.yml -f compose.veille.yml run --rm veille run --once

FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY .sqlx ./.sqlx
COPY migrations ./migrations
COPY src ./src
# Offline sqlx metadata: no database needed at build time.
ENV SQLX_OFFLINE=true
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 900 --home /var/lib/veille veille \
    && mkdir -p /var/lib/veille /etc/veille \
    && chown veille:veille /var/lib/veille
COPY --from=build /src/target/release/veille /usr/local/bin/veille
USER veille
# No entrypoint loop and no scheduler: the operator's timer invokes
# `veille run --once` (PRP §1: not a scheduler).
ENTRYPOINT ["/usr/local/bin/veille"]
CMD ["--help"]
