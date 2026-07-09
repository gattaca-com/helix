toolchain := "nightly-2025-09-18"

fmt:
  rustup toolchain install {{toolchain}} > /dev/null 2>&1 && \
  cargo +{{toolchain}} fmt

fmt-check:
  rustup toolchain install {{toolchain}} > /dev/null 2>&1 && \
  cargo +{{toolchain}} fmt --check

clippy:
  cargo +{{toolchain}} clippy --all-features --fix --allow-dirty --no-deps -- -D warnings

test:
  cargo test --workspace --all-features

admin-frontend-build:
  cd crates/admin/frontend && npm ci && npm run build

admin-build: admin-frontend-build
  cargo build --release -p helix-admin --bin admin

admin-run config="admin-config.yml": admin-frontend-build
  cargo run -p helix-admin --bin admin -- --config {{config}}

local-postgres:
  docker run -d --name helix-postgres -e POSTGRES_PASSWORD=password -p 5432:5432 timescale/timescaledb-ha:pg17

local-setup:
  just local-postgres

local-clean:
  docker rm -f helix-postgres
