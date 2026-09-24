FROM lukemathwalker/cargo-chef:latest-rust-1.96 AS chef
WORKDIR /app

# Before `cook`: build deps installed after it miss the cached layer.
RUN apt-get update && apt-get install -y \
  clang \
  protobuf-compiler \
  && rm -rf /var/lib/apt/lists/*

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
COPY crates/vendored ./crates/vendored
RUN cargo chef cook --profile release-prod --recipe-path recipe.json -p helix-relay --bin helix-relay

COPY . .
RUN cargo build --profile release-prod -p helix-relay --bin helix-relay --bin data-gatherer

FROM debian:stable-slim AS runtime
WORKDIR /app

RUN apt-get update && apt-get install -y \
  ca-certificates && \
  rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release-prod/helix-relay ./
COPY --from=builder /app/target/release-prod/data-gatherer ./
COPY relay-entrypoint.sh ./

ENTRYPOINT ["/app/relay-entrypoint.sh"]
