toolchain := "nightly-2025-02-26"

fmt:
  rustup toolchain install {{toolchain}} > /dev/null 2>&1 && \
  cargo +{{toolchain}} fmt

fmt-check:
  rustup toolchain install {{toolchain}} > /dev/null 2>&1 && \
  cargo +{{toolchain}} fmt --check

clippy:
  cargo clippy --all-features --no-deps -- -D warnings

test:
  cargo test --workspace --all-features

local-setup:
  docker run -d --name helix-postgres -e POSTGRES_PASSWORD=password -p 5432:5432 timescale/timescaledb-ha:pg17 && \
  docker run -d --name helix-redis -p 6379:6379 redis/redis-stack-server:latest

local-clean:
  docker rm -f helix-postgres helix-redis
