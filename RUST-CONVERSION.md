# Munder Difflin → Rust, as a multi-tenant web server

## What is being converted

| Layer | Today | After |
|---|---|---|
| `src/main` | 21.4k LOC Electron main (Node) | `md-server` — axum + tokio |
| `src/preload` | 1.4k LOC `window.cth` bridge | Generated typed client over HTTP + WebSocket |
| `src/shared` | 4.9k LOC TS types | `md-contract` — serde types, generated from the manifest |
| `src/renderer` | 36.5k LOC React / Pixi.js / xterm.js / Monaco | `md-ui` — Leptos, compiled to WASM |

The seam between the halves is small and fully enumerated: **158 RPC channels,
3 synchronous RPC, 28 push channels** (3 per-instance PTY streams). That surface
is extracted to `contract/manifest.json` and both sides generate from it, so the
wire format has one source of truth and drift becomes a compile error.

## Sequencing

The port runs **backend-first**. The Rust server reaches parity while the existing
Electron renderer still drives it — pointed at HTTP instead of IPC — so the backend
is provably correct before any UI is rewritten. The WASM UI then replaces a working
frontend rather than being written against a moving target. Cutover happens once,
at the end.

```
Phase A  contract + workspace + axum core          ← foundation
Phase B  tenancy + sandboxed execution             ← must precede any agent spawning
Phase C  terminal plane → data plane → hive → integrations
         (Electron renderer runs against the Rust server here: parity checkpoint)
Phase D  WASM UI: shell → terminal → floor → editor
Phase E  cutover, delete Node/Electron
```

### Phase A — foundation
Cargo workspace (`md-contract`, `md-server`, `md-pty`, `md-hive`, `md-ui`), the
axum core, `POST /rpc/:channel` for the 158 calls, `/ws` for push and PTY streams.
Auth and the `TenantId` extractor land here, not later: retrofitting authorization
onto 158 endpoints is how endpoints get missed.

### Phase B — tenancy and the sandbox
The product spawns arbitrary CLI agents with filesystem access. Multi-tenant, that
is remote code execution as the server user, so isolation has to be OS-level —
path prefixing is not a security boundary. A pluggable `Sandbox` trait with three
backends:

- `passthrough` — single-user dev, current behavior
- `local-uid` — one unix user per tenant, agents spawned via `setuid`
- `container` — podman/docker per tenant, the deployable default

Everything tenant-scoped hangs off this: harness home, roster, hive, PTY namespace,
resource quotas. **No agent spawning ships before this phase closes.**

### Phase C — port the backend, plane by plane
In dependency order, each plane green before the next starts:

1. **Terminal** (~1.5k LOC) — `pty.ts`, `ptyEnv.ts`, `procKill.ts`, `shellEnv.ts` →
   `portable-pty`. Preserves login-shell PATH capture, the hive-node runtime
   fallback, process-tree kill, and the `lastOutputAt`/`hasOutput` idle handshake
   that gates typing into a live PTY.
2. **Data** (~2.5k LOC) — `config`, `fs`, `git`, `db`, `memory`, `knowledge`,
   `kg-core` → `rusqlite` + `git2`. On-disk harness-home layout preserved so
   existing installs migrate without conversion.
3. **Hive** (~4.5k LOC) — `hive.ts` alone is 3.3k: message router, provider outbox
   draining, GOD adjudication, idle/inbox wakeups, plus the HTTP hook server
   provider bridges POST to. Largest single unit; budget accordingly.
4. **Integrations** — slack, webhooks, triggers, github, skills, missions. The
   write-only secret contract holds: values never cross the wire, only `hasSecret`.

### Phase D — the WASM UI
The three heavy UI dependencies have no drop-in Rust equivalent and are the
schedule risk. Each gets a prototype-and-benchmark gate before commitment:

| Replacing | With | Risk |
|---|---|---|
| xterm.js | `alacritty_terminal` VT engine + canvas/WebGL renderer | **High** — must match xterm's throughput on agent output |
| Pixi.js floor | `wgpu` (WebGL2/WebGPU) tilemap | Medium — existing `.tmj` maps and tilesets reused as-is |
| Monaco / CodeMirror | `ropey` + `tree-sitter` | **High** — Monaco parity is not a realistic target; scope deliberately |

If a gate fails, the fallback is a JS island for that widget behind a Leptos
component boundary — the rest of the UI stays Rust.

### Phase E — cutover
Both stacks run against the same harness home and are diffed channel by channel,
with the manifest as the checklist. Then `src/main`, `src/preload`, `src/renderer`,
electron-builder, and the Node toolchain are deleted, and packaging becomes Rust
binary builds.

## Honest risk register

- **The terminal is the product.** Agent output is high-volume and escape-sequence
  dense. xterm.js has had years of tuning. This is the single most likely place to
  regress and it is on the critical path — prototype it early, in Phase A if
  possible, not when Phase D starts.
- **`hive.ts` carries undocumented behavior.** 3.3k LOC of routing, adjudication,
  and wakeup timing. Port it against recorded traces from the running Electron app,
  not against a reading of the source.
- **Multi-tenant changes the product's threat model**, not just its plumbing.
  Local-first meant the user's own machine ran their own agents. A shared server
  means one tenant's agent must not reach another's files, processes, or secrets.
- **Behavior changes that need product decisions**, surfaced by the port rather
  than caused by it: agents now outlive the browser tab; reveal-in-Finder and
  open-in-Terminal act on the server; the auto-updater goes away.

See `contract/PORTING-NOTES.md` for the 21 Electron-native channels and the 3
synchronous ones, each with its substitute.
