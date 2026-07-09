FROM node:22-slim AS frontend
WORKDIR /frontend
COPY crates/admin/frontend/package.json crates/admin/frontend/package-lock.json ./
RUN npm ci
COPY crates/admin/frontend/ .
RUN npm run build

FROM lukemathwalker/cargo-chef:latest-rust-1.91 AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
# rust-embed picks the assets up at compile time in release builds, so the
# frontend must be in place before cargo build runs
COPY --from=frontend /frontend/dist crates/admin/frontend/dist
RUN cargo build --release -p helix-admin --bin admin

FROM debian:stable-slim AS runtime
WORKDIR /app

RUN apt-get update && apt-get install -y \
  ca-certificates && \
  rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/admin ./

ENTRYPOINT ["/app/admin"]
