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

## Engines and tools

**Engines** are the agent CLIs a floor can hire — a name, a command, its
arguments, and whether the CLI speaks Claude Code's hook and settings protocol.
Fourteen ship (`crates/md-server/src/engines.rs`); a tenant registers its own or
overrides any field of a built-in under `engines` in its config, from
setup → engines. Only Claude Code is marked hooked, because the original's
translating shims for the others are not ported and an engine that claims hooks
it lacks looks live on the floor and reports nothing.

An engine also carries an **environment**, which is how an OpenAI-wire CLI is
pointed at a model server — and therefore where its API key would go. Keys do
not go in the config with the address. Any environment name that looks like a
credential (`KEY`, `TOKEN`, `SECRET`, `PASSWORD`, `CREDENTIAL` — see
`engines::is_secret`) is encrypted into the tenant's secret store, the config
keeps only the NAME under `envSecrets`, and no response carries the value: the
panel can write a credential and can never read one. It is decrypted in exactly
one place, `pty_spawn`, into the agent's own environment.

Three consequences worth knowing:

* **No `MD_SECRET_KEY`, no credential.** The save is refused rather than
  downgraded to plaintext, and the panel says storage is off instead of offering
  a button that always fails. This is the rule `secrets.rs` already held for
  integration tokens.
* **A store that cannot produce what the config names stops the spawn.** An
  agent started without its key does not look broken — it looks hired, and
  reports a refusal from a server minutes later into a terminal nobody is
  reading.
* **A key entered before any of this existed is migrated, not nagged about.**
  The panel marks it "in the clear"; saving that engine moves it into the store
  and drops it from the config, without anyone having to find the value again.

Catalogue defaults are exempt: LM Studio ships `OPENAI_API_KEY=lm-studio`, which
is printed in `engines.rs` and is not a secret whatever it is called.

**Tools** are MCP servers (`crates/md-server/src/mcp.rs`), registered the same
way under `mcpDefaults`. The consent tiers are load-bearing: `safe-readonly`
ships on, everything else needs an explicit `enabled: true`, and a registration
the bundle has not vetted is treated as `write` unless it says otherwise.

The orchestrator can register one too, by writing a message to the reserved
recipient `harness`:

```json
{ "to": "harness", "act": "propose",
  "tool": { "id": "scraper", "label": "Scraper",
            "command": "npx", "args": ["-y", "some-server"] } }
```

What it registers is **always off**, always `write`, and always tagged with who
asked. It may not name a tool that already exists, because `filesystem` ships
armed and an agent that could rewrite its command would have a shell rather than
a tool. Only the orchestrator may ask, and it is told the answer will need a
person. See `handlers::propose_tool` — the whole decision is one function so it
can be read in one go.

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
