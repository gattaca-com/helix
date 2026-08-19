# helix-admin

Standalone admin website for relay operators: a React SPA served by an axum
binary, with read-only dashboards backed by Postgres and admin actions
(kill switch, builder demote/promote, disable adjustments) proxied to the
relay's admin API on port 4050.

## Running

```sh
# one-off: build the frontend into frontend/dist
just admin-frontend-build

ADMIN_TOKEN=<token> cargo run -p helix-admin -- --config admin-config.yml
```

See `admin-config.example.yml` at the repo root. `ADMIN_TOKEN` is used both to
authenticate requests to this service and (unless `RELAY_ADMIN_TOKEN` is set)
to call the relay's admin API.

## Frontend development

```sh
cd crates/admin/frontend
npm install
npm run dev   # Vite on :5173, proxies /api to the backend on :4060
```

## Asset embedding

The SPA build output (`frontend/dist`) is served via rust-embed. In debug
builds the files are read from disk at runtime; in release builds they are
embedded into the binary at compile time — so anything that produces a release
binary (including `admin.Dockerfile`) must build the frontend *before*
`cargo build --release`. A fresh checkout compiles fine without Node: `dist`
contains only `.gitkeep` and the server answers 503 for UI routes until the
frontend is built.
