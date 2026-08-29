# md-server

The Rust backend replacing the Electron main process. See `../RUST-CONVERSION.md`
for the plan and `../contract/PORTING-NOTES.md` for the channels that need
redesign rather than porting.

## Run

```sh
MD_DATA_ROOT=./data MD_SANDBOX=passthrough cargo run -p md-server
```

| Variable | Default | Meaning |
|---|---|---|
| `MD_BIND` | `127.0.0.1:7777` | Listen address |
| `MD_DATA_ROOT` | `./data` | Parent directory holding every tenant home |
| `MD_SANDBOX` | `passthrough` | `passthrough` \| `container` |
| `MD_CONTAINER_RUNTIME` | `podman` | Runtime for the container sandbox |
| `MD_AGENT_IMAGE` | `munderdifflin/agent:0.4.6` | Image agents run in |
| `MD_STATIC_DIR` | unset | Serves the built WASM client when set |
| `MD_USER` / `MD_PASSWORD` / `MD_TENANT` | `dev` | Bootstrap dev account |

`passthrough` provides no isolation and **refuses to start with more than one
tenant**. Any real deployment uses `container`.

## Shape

```
md-contract   channel enum generated from ../contract/manifest.json + wire types
md-tenant     TenantId, per-tenant paths, the Sandbox trait and its backends
md-pty        portable-pty session manager and byte streaming
md-server     axum: /rpc/{channel}, /ws, auth, dispatch
```

## Port progress

`/api/health` reports ported vs total channels. For the remaining list:

```sh
cargo test -p md-server port_coverage -- --nocapture
```

Adding a channel means writing a handler and adding one arm to `handler_for` in
`rpc.rs` — that function is the only place a channel binds to an implementation,
so coverage is never a separate list that can drift.

## Test container

```sh
docker compose -f rust/compose.yaml up -d --build   # host 127.0.0.1:9876
docker compose -f rust/compose.yaml logs -f
docker compose -f rust/compose.yaml down            # add -v to drop the data volume
```

Build context is the repo root, not `rust/`: `md-contract`'s build script reads
`contract/manifest.json`, so the manifest must sit beside the workspace in the
image exactly as it does in the repo. `.dockerignore` keeps everything else out
of the context.

The published port is `127.0.0.1:9876` rather than `9876`. This server spawns
agent processes, so a bare port mapping would offer that to the whole local
network. Widen it deliberately or not at all.

Sessions are held in memory, so a restart requires a fresh login; tenant data
lives in the `md-data` volume and survives.

Smoke test:

```sh
curl -s localhost:9876/api/health
TOKEN=$(curl -s -X POST localhost:9876/api/login \
  -H 'content-type: application/json' \
  -d '{"user":"dev","password":"dev"}' | jq -r .token)
curl -s -X POST localhost:9876/rpc/app:info \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' -d '[]'
```
