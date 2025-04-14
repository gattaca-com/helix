FROM lukemathwalker/cargo-chef:latest-rust-1.82 AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Install build dependencies
RUN apt-get update && apt-get install -y \
  clang \
  protobuf-compiler \
  && rm -rf /var/lib/apt/lists/*

COPY . .
RUN cargo build --release --bin helix

FROM debian:stable-slim AS runtime
WORKDIR /app

RUN apt-get update && apt-get install -y \
  ca-certificates && \
  rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/helix ./

ENTRYPOINT ["/app/helix"]
