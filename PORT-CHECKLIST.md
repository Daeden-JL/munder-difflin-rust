# Port checklist

**Status: 40 of 161 RPC channels ported.** Regenerate this count any time:

```sh
cd rust && cargo test -p md-server port_coverage -- --nocapture
```

Done so far: the contract extractor, the Cargo workspace, the axum core
(auth + tenancy + RPC + WebSocket), the sandbox trait with two backends, the PTY
core, a LAN-reachable test container, the git plane (14 channels), the hive
STATE layer (14 channels), and a dev console at `/`. Everything below is what remains.

---

## How much until it looks like the desktop app?

Not 161 channels — **50**. That is the unported set reachable from the main
screen's own call sites (`App`, the store, `useHive`, `AgentStrip`, `AgentCard`,
`CommandCenterPanel`, `OfficeFloor`, the composer, the pty views), as opposed to
110 unported across the whole renderer including every modal and settings tab.

Measured, not estimated:

```sh
python3 contract/first-screen.py
```

The gap is dominated by one namespace:

| namespace | unported | what it blocks |
|---|---|---|
| `app:` | 10 | mostly Electron-native; see `contract/PORTING-NOTES.md` |
| `hive:` | 10 | **7 are subscriptions** — they need the hook server, not more state |
| `control:` | 7 | pause/resume/halt/steer |
| `pty:` | 4 | the three push streams + `redraw` |
| `hire:` | 3 | agent creation |
| everything else | 16 | ten namespaces, 1–2 channels each |

The floor can now be *drawn* — registry, tasks, board, memory and inboxes all
read. What it cannot yet do is *update itself*: every remaining hive channel bar
three is a push subscription fed by the hook server. So the next unit of work is
the event source, not more state.

---

## Phase C — backend port (the bulk of the work)

### ☐ C1. Finish the terminal plane — `pty.ts` 806 + 272 LOC of helpers
The core spawn/stream/kill path works. What is missing is everything that makes
an agent CLI actually start on a real machine.

1. Port `shellEnv.ts` (128) — capture PATH from a login shell. Without it, agents
   started by the server see a stub PATH and die with 127.
2. Port `ptyEnv.ts` (90) — environment construction for the child.
3. Port `procKill.ts` (54) — process-**tree** kill. Killing the pty leader alone
   leaves MCP servers and helper processes orphaned.
4. Port `withHiveRuntimeFallback` — appends the bundled-node dir to PATH.
   Append, never prepend: prepending swaps the node version under the user's own
   projects, which the TS comments call out as a deliberate product decision.
5. Port session resume + `session:resolveCwd`, and the relaunch-after-install
   path that re-arms a terminal in place (`pty:relaunch`).

**Done when:** a real `claude` process starts in the container, survives a
resize, and a tree-kill leaves no orphans (`ps` clean).

### ☐ C2. Data plane — ~2.3k LOC → `rusqlite` + `git2`
1. `config.ts` (827) → `config:*` (4 channels). Largest non-hive module; mostly
   defaults, migration, and validation.
2. `fs.ts` (229) → remaining `fs:*`. Note `fs:revealPath` is Electron-native, see
   `contract/PORTING-NOTES.md`.
3. ~~`git.ts` (489) → `git:*` (**14 channels**)~~ — **done.** Shells out rather
   than binding `git2`: the renderer parses these exact shapes today, so
   byte-identical output mattered more than elegance. The two guards were ported
   deliberately, not incidentally — `is_safe_rev` (a ref beginning `-` becomes a
   flag, i.e. command execution) and `safe_join` (a repo-relative path must not
   climb out), both with tests. `git` is now installed in the runtime image;
   without it every one of these channels fails at runtime, not at build.
4. `db.ts` (175) + `memory.ts` (447) + `knowledge.ts` (122) + `kg-core.cjs` (359)
   → `kg:*` (7) and `memory:*`. Keep the on-disk schema byte-compatible so an
   existing harness home opens unchanged.

**Done when:** an existing harness home copied into a tenant dir opens with its
config, git state, and memory intact.

### ◐ C3. Hive plane — ~4.7k LOC, the single largest unit
`hive.ts` alone is 3,303 LOC and backs **23 channels**. Split in two, because
the halves have nothing in common: **state** (files on disk) and **events**
(things that happen). State is done; events are not.

**☑ C3a. State — 14 channels.** `registry`, `board`, `tasks`, `addTask`,
`patchTask`, `deleteTask`, `log`, `memory`, `inbox`, `renameAgent`,
`patchAgentRole`, `setArchived`, `setAgentHold`, `send`.

The shaping constraint is that these files are **hand-written by the god agent**,
which appends fields no UI models — `result`, `repo`, `scope`, `origin`,
`commit`. `src/shared/taskLedger.ts` exists because a writer holding a partial
model once deleted every unmodelled field on every card the moment a user
touched one. So the Rust store reads and writes `serde_json::Value`, never typed
structs: **typing these records would reintroduce that exact bug.** Same rule for
registry records — a rename must not drop `sessionId`, which is the `--resume`
key. Both are tested.

Also ported: the router (hop cap, broadcast fan-out, `human`/`god` resolution,
the assistant bounce), atomic write-then-rename (agents poll these files, and a
truncate-in-place is readable as empty mid-write), and a process-wide write lock
standing in for the git single-committer's ordering.

Mail to a provider with no inbox drain (`kimi`, `copilot`, `custom`) bounces to
the god with an `[undeliverable …]` subject. That is the Electron *fallback*
path, taken because the terminal work-order channel is not ported — loud rather
than silent, and it becomes the primary path again once handoff lands.

**☐ C3b. Events — 9 channels, 7 of them subscriptions.**
1. Port `hooks.ts` (345): the socket server provider bridges POST lifecycle
   payloads to (`cth-hook`, `agy-hook`). Decide whether it shares the axum
   server or binds its own listener per tenant. **This is now load-bearing for
   the UI** — see the D0 decision; the conversation view is built from hook
   events plus the transcript, not from screen-scraping.
2. Port `transcript.ts` (341) — the session JSONL reader.
3. Port `control.ts` (128) + `breaker.ts` (347) → `control:*` (7 channels).
4. Port `reflect.ts` (437) → memory condensation; `hive:searchMemory` /
   `hive:textSearch` / `hive:agentContext`.
5. Port spawn/provisioning from `hive.ts`: agent directories, MCP config, the
   hook shim, `roster.ts` (204).
6. Restore the git single-committer (retry/backoff + stale-lock recovery); the
   write lock is a same-process stand-in, not a replacement.

**Do not port this from a reading of the source.** Record traces from the running
Electron app and replay them; the routing and wakeup timing carry behavior the
code does not state.

**Done when:** two agents exchange messages, GOD adjudicates, and an idle worker
is woken — matching a recorded Electron trace.

### ☐ C4. Integrations plane — ~2.2k LOC
`slack.ts` (508), `webhook.ts` (439), `skills.ts` (467), `integrationBroker.ts`
(311), `integrations.ts` (156), `triggerHistory.ts` (153), `github.ts` (122).
Covers `slack:*` (6), `integrations:*` (6), `webhook(s):*` (10), `skills:*` (5),
`triggers`/`org`/`missions`/`hire`.

Preserve the write-only secret contract: secret values never cross the wire, only
`hasSecret`. It is enforced today in `listRecordsRedacted()` — port the redaction
with the feature, not after it.

**Done when:** a Slack message reaches an agent and a webhook trigger fires, with
no secret value present in any response body.

### ☐ C5. Electron-native surface — 21 channels, mostly deletion
Each needs a decision, not a port. Full table in `contract/PORTING-NOTES.md`.
- Clipboard (3) → async Clipboard API
- Pickers (2) + `pathForFile` → server-side directory browser + upload
- Shell integration (3) → `openExternal` becomes a link; reveal/open-in-Terminal
  act on the *server* and should probably be dropped
- App lifecycle (4) → agents now outlive the browser tab; needs a product call
- Auto-updater (7) → **delete**; replace with a "server upgraded, reload" push
- The 3 synchronous calls → prefetch into app state

### ☐ C6. Parity checkpoint
Point the **existing Electron renderer** at the Rust server over HTTP and run the
app. This is the gate: the backend must be proven before any UI is rewritten.

---

## Phase D — the WASM UI

### ☑ D0. Terminal emulator — **descoped** (decision, 2026-08-29)
The brief is a composer for describing tasks, not a terminal. That removes what
was the highest-risk item in the conversion — no `alacritty_terminal`, no
throughput benchmark against xterm.js, no JS-island fallback to plan for.

It is a sound trade because the desktop app's own code says so. `usePtyParser.ts`
derives every avatar state by **screen-scraping the TUI** — regexing `● Read x`,
`esc to interrupt`, `Do you want to proceed` — and its header calls itself "a
stopgap until we wire real Claude Code hooks". Those hooks already exist:
`hooks.ts` (345) runs a Unix-socket server fed by `PreToolUse` / `PostToolUse` /
`Notification`, and `transcript.ts` (341) reads the session JSONL. So the
structured source the scraper approximates is already there.

What replaces the terminal:

1. **In** — composer → queue → `pty:write`. The PTY stays as transport; it is
   only never *rendered*.
2. **Out** — a conversation view built from the transcript JSONL plus hook
   events. Strictly better than scraping: real tool names and arguments instead
   of regexed glyphs.
3. **Approvals** — still need a reply path. `blockReason.actions` already sends
   `y\r` / `n\r`, so the buttons work without an emulator.

Two things genuinely lost, both worth stating rather than discovering later:
a user cannot drop into raw shell interaction, and any agent CLI *without*
hooks (a plain `bash`, a non-Claude engine) has no structured output at all and
would show only stripped text. Keep the dev console's raw sink for that case.

**Prerequisite this creates:** C3 must land the hook server and transcript
reader, which were previously "nice to have" behind the terminal. They are now
load-bearing for the UI.

### ☐ D1. Leptos shell + generated client
Scaffold `md-ui`, generate a typed client from `contract/manifest.json`, add a
reconnecting WebSocket, port the zustand store to signals.

### ☐ D2. Port the React tree — ~36.5k LOC, 80 components
The long tail. Sequence by dependency: layout → command centre → agent list →
panels. `FullscreenTerminal.tsx` (1052) and `terminalPool.ts` (1000) are dropped
by the D0 decision; `PtyTerminalView.tsx` (419) shrinks to a transcript view.

### ☐ D3. Office floor — Pixi.js → `wgpu`
Tilemap, avatars, animation. Existing `.tmj` maps and tilesets are reused as-is.

### ☐ D4. Editor — Monaco/CodeMirror → `ropey` + `tree-sitter`
Scope deliberately; Monaco parity is not a realistic target. Languages currently
configured: css, html, javascript, json, markdown, python, yaml.

---

## Phase E — cutover

### ☐ E1. Channel-by-channel parity diff
Both stacks against the same harness home, `manifest.json` as the checklist.

### ☐ E2. Delete the Node stack
`src/main`, `src/preload`, `src/renderer`, `electron-builder.yml`,
`electron.vite.config.ts`, `package.json`, and `contract/extract-contract.mjs`
(its input is gone). Replace packaging with Rust binary builds.

---

## Cross-cutting, not tied to one phase

- ☐ **`LocalUid` sandbox backend** — the trait and Container backend exist;
  this one is unimplemented.
- ☐ **Session persistence** — sessions are in-memory, so a restart logs everyone
  out. Fine for testing, not for deployment.
- ☐ **Account store** — accounts are currently constructed at boot from env vars.
  Needs a real control-plane store with signup/rotation.
- ☐ **WebSocket origin allowlist** — `SameSite=Strict` covers the cookie path,
  but the handshake `Origin` is not validated.
- ☐ **Rate limiting / quotas** — nothing caps a tenant's PTY count or CPU.
- ☐ **Replace the self-signed cert** with a real one if this outlives testing.
