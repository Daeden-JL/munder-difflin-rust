# Port checklist

**Status: 67 of 161 RPC channels ported, 20 deliberately never ported, 74 to go.
10 of 28 push channels served.** Regenerate this count any time:

```sh
cd rust && cargo test -p md-server port_coverage -- --nocapture
```

Done so far: the contract extractor, the Cargo workspace, the axum core
(auth + tenancy + RPC + WebSocket), the sandbox trait with two backends, the PTY
core, a LAN-reachable test container, the git plane (14 channels), the hive
state layer (14), the hook server + operator control (7 RPC + 3 push), agent
provisioning (claude only, +2 push), closing time, the outbox router, the
`md-hook` shim, and a dev console at `/`.

**Channel accounting changed.** `unported()` used to count channels that will
never be ported — clipboard access, the desktop auto-updater, the app's own
window — so the number could not reach zero and nobody could tell how much of
the remainder was real work. `rpc::plan()` now classifies every channel as
`Server` / `Client` / `Dropped` / `Todo`, and a client calling a written-off
channel gets `not_applicable` with the reason, not `not_implemented`:

```
app:copyToClipboard  → the client's job: use the async Clipboard API
fs:revealPath        → no server-side meaning: would act on the server
update:*             → no server-side meaning: the server updates out of band
```

`/api/health` reports `todoChannels` and `notApplicableChannels` separately. Everything below is what remains.

---

## How much until it looks like the desktop app?

Not 161 channels — **23**. That is the unported set reachable from the main
screen's own call sites (`App`, the store, `useHive`, `AgentStrip`, `AgentCard`,
`CommandCenterPanel`, `OfficeFloor`, the composer, the pty views), as opposed to
75 unported across the whole renderer including every modal and settings tab.

Measured, not estimated:

```sh
python3 contract/first-screen.py
```

The gap is dominated by one namespace:

| namespace | unported | what it blocks |
|---|---|---|
| `hive:` | 5 | 3 search, `enqueueToAgent`, `terminalHandoff` |
| `hire:` | 3 | agent creation |
| everything else | 15 | eleven namespaces, 1–2 channels each |

The floor can be drawn, it updates itself, agents can be spawned into it, they
can talk to each other unattended, and the floor can be wound down gracefully.
What is left is a long tail: no namespace now accounts for more than a quarter
of it, so the remaining work is wide rather than deep.

---

## Phase C — backend port (the bulk of the work)

### ☑ C1. Terminal plane — done
1. **PATH** (`shellEnv.ts`). A server started by systemd, launchd or Docker has
   a minimal PATH that usually lacks whatever installed the agent CLI, so a bare
   `claude` exits 127 — which reads as "the agent crashed", not "PATH is wrong".
   Captured from an interactive login shell, **fenced between two markers**:
   rc files are free to print, and a zsh session plugin emitting
   `Restored session: …` before the script silently poisons the value. A
   multi-line result is rejected for the same reason. `MD_AGENT_PATH` overrides
   it — in a container the image's PATH is already right and spawning a login
   shell per boot is waste.
2. **Bundled-runtime fallback** appends, never prepends: prepending would shadow
   the version a user's own project pins.
3. **Process-tree kill** (`procKill.ts`). Polite signal first, then after a 4s
   grace, SIGKILL the process **group**. A bare kill signals the direct child
   only, so a child that ignores it never dies and its own children — MCP
   servers, helper daemons — are orphaned to PID 1. Escalation runs on a timer,
   not inline, so the caller is not blocked for the grace.
   Verified against the exact leak: a `bash -c 'trap "" HUP; sleep 900 & sleep
   900'` tree survives the polite kill (3 processes still up after 1s) and is
   gone after the escalation (0).
4. `session:resolveCwd` and `pty:redraw`. Redraw resizes a column narrower and
   back — there is no portable redraw signal, and a size change is what every
   terminal program already listens for.
5. Session **resume** landed with provisioning: the id comes from the registry
   where the hook server writes it, so a resume works after a crash without the
   client remembering anything.

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
4. ~~`knowledge.ts` + `kg-core.cjs` → `kg:*`~~ — **done.** File-backed, not
   sqlite, deliberately: the store is a directory an agent CLI reads
   out-of-process, so **the layout IS the interface** and moving it into a
   database would break every agent that reads it directly.
   `index.jsonl` (one line per chunk) + `docs/<id>/{meta.json,text.md}`.

   Keyword scoring ported to match the original's ranking exactly — log-damped
   term frequency, title boost, breadth bonus, exact-phrase bonus — because
   agents and the UI search the same store, and disagreeing about relevance
   would be worse than either ranking alone. Ties break on docId then chunk
   index so identical queries return identical order.

   `kg:ingestFiles` resolves every path through the tenant guard: it is the one
   channel that reads arbitrary files at the client's request. Binary files are
   refused rather than indexed as mojibake. `kg:addFiles` opens a native picker
   and is therefore the client's job.

5. ~~`db.ts` (175) → command history~~ — **done**, as an append-only JSONL log
   rather than sqlite. History is append, read-the-tail, substring-search; a log
   is that shape already, and it avoids a database for one table.

**Still open:** `memory.ts` (447) condensation beyond `memory:reflectNow`, which
currently asks the agent to condense its own memory — that is the agent's work,
not the harness's, since only it knows which notes still matter.

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

**☑ C3b. Hook server + operator control — 7 RPC + 3 push.**
One Unix socket per tenant at `<hiveRoot>/hooks.sock`, mode 0600. **The socket
path is the authorization** — it lives inside the tenant's own home, so there is
no token to check and no way to name another tenant's agent.

Boundaries are published to the tenant's WebSocket hub (`hive:hookEvent`,
`hive:contextUpdate`, `control:approvalRequest`), and the reply rides Claude
Code's own hook-return protocol: `permissionDecision: deny` for a paused or
gated agent, `additionalContext` for a steer, `continue: false` for a halt.

Order in `hooks::handle` is load-bearing and commented at each branch. Two that
are easy to get wrong: `Status` is handled FIRST and returns early (it is
telemetry, so a halted agent's status line must not answer `continue: false`
forever), and `Stop` never converts unread mail into a forced continuation —
that path bypasses the HITL safety and can spend credits mid-answer.

Also shipped: `md-hook`, the shim the agent runs. It replaces `cth-hook.cjs`,
which needed a Node runtime inside the agent environment; this is a static
binary with **no dependencies**, since it runs many times per turn and its cost
is startup time. It is a byte relay that never parses the JSON — the payload
schema belongs to the CLI and the reply to the server. **Every failure path
prints `{}` and exits 0**: a harness that is down must be invisible to the work,
not a wedge in front of it.

Known gap: mode 0600 is right for Passthrough and Container, where the agent
shares the server's uid. `LocalUid` needs a shared group and 0660 — noted at the
`restrict()` call site.

**◐ C3c. Spawn lifecycle — claude done, other providers not.**
`pty:spawn` with a `hive` block now provisions the agent: workspace
(`inbox/.done`, `outbox/.sent`, `identity.md`, `memory.md`, `cursor.json`),
registry upsert, and a per-session `settings.json` pointing every lifecycle hook
at `md-hook`. Fires `hive:agentSpawned` (after the pty is actually up, so the
floor never draws an agent that failed to start) and `hive:agentArchived`.

Verified as a full loop: spawn → the CLI reports `SessionStart` through the shim
→ the registry records `sessionId` → a respawn with `requireResume` attaches
`--resume <id>`.

Rules worth keeping when this is extended:
- `identity.md` is refreshed every spawn (generated, so a stale copy lies);
  `memory.md` is seeded ONCE (durable, so a rewrite would erase what the agent
  learned).
- The registry upsert spreads the PRIOR entry first, or a respawn wipes
  `sessionId` and every restart silently begins a fresh thread.
- A status-like caption ("on standby") must never overwrite a durable hire role;
  a restart passes the roster's caption, so this fires on the common path.
- An unported provider is REFUSED, not half-provisioned — an agent without hooks
  looks live on the floor while reporting nothing.
- Resume is resolved BEFORE provisioning, so a failed `requireResume` leaves
  nothing behind. (Electron provisions first and leaves a registry entry for an
  agent that never started; the lookup does not need that order, because
  `sessionId` is only ever written by the hook server.)

### ☑ C5. Electron-native surface — resolved as 19 written-off channels
Not ported, deliberately, each with a reason in `rpc::plan()`. Seven are the
browser's job (clipboard ×4, `openExternal`, notifications, the two file
pickers); twelve have no meaning for a remote tenant (the app's own window,
login item, reveal-in-file-manager, open-in-terminal, and the six
`electron-updater` channels).

The one piece of real product behaviour in that group was **closing time**, and
it is ported. It is the graceful, data-loss-free shutdown: the human announces
it, every worker parks its work and appends state to `memory.md`, the god
collects the ACKs and concludes.

**Behaviour change, on purpose.** In Electron the protocol ended in `app.quit()`.
Here that would take every other tenant's floor down with it, so conclusion kills
**this tenant's** PTY sessions and nothing else. It follows that closing a
browser tab must not start this — a tenant's agents keep running until someone
deliberately closes the floor.

Two rules the original earned and this keeps:
- Only agents with a LIVE pty are waited on. The registry keeps records for
  agents that died with a crash, so a registry-based roster waits forever on
  something that can never ACK.
- A premature `CLOSING-TIME-COMPLETE` is REJECTED, with the missing workers
  named. The god is told to wait for every ACK, but the entire point is that no
  worker loses unsaved state, so the harness verifies independently. A worker
  whose terminal died mid-protocol is excused — its ACK can never arrive.

Steer notes carry the announcement to deeply busy agents, since the inbox brief
only lands when one next stops; a worker hours into a task would otherwise hold
the whole shutdown.

**☑ C3d. The outbox router — `hive:message`.**
One polling task per tenant sweeps `agents/*/outbox/*.json` every 1.5s, routes
each message, and archives it to `outbox/.sent` so a crash cannot deliver it
twice. Polling rather than filesystem watching: agents write these files by hand
from arbitrary processes, and a poll is cheap and does not depend on platform
watch semantics. This is what lets agents talk to each other with no client
attached.

Two rules:
- **The owning directory is authoritative for `from`.** An agent hand-writes
  these files, so a self-declared sender would let any agent post as any other.
  Verified: a message written into `jim/outbox` claiming `"from":"michael"`
  arrives as from `jim`.
- **A file that will not parse is left alone until it stops changing.** The
  poller can catch a hand-written file mid-write; the Electron original
  quarantines it immediately as `bad-*`, which throws away a message that was
  about to be valid. Here it is given a 3s grace, then quarantined so a
  genuinely broken file is not retried forever.

**☐ C3e. Remaining hive — 5 channels + the other providers.**
1. Provider bridges: `agy`, `codex`, `pi` (config-file hook shims) and the
   `qwen` reverse-proxy sidecar. Each has its own shim and config layout.
2. Port `transcript.ts` (341) — the session JSONL reader. Load-bearing for the
   conversation view; see the D0 decision.
3. Port `breaker.ts` (347) → `control:breakerState` / `control:setBreakerState`.
4. Port `reflect.ts` (437) → `hive:searchMemory` / `hive:textSearch` /
   `hive:agentContext`.
5. Port `emitTerminalHandoff` → `hive:terminalHandoff` and
   `hive:enqueueToAgent`, which restore the PRIMARY delivery path for hookless
   providers; today they bounce to the god.
6. Restore the git single-committer (retry/backoff + stale-lock recovery); the
   write lock is a same-process stand-in, not a replacement.

**Debugging note.** `md-hook` fails OPEN — any error prints `{}` and exits 0, so
a broken harness never wedges an agent. The cost is that a failure looks like a
successful no-op. When hooks appear to do nothing, read the shim's **stderr**;
it names the real cause (this bit during testing, when the socket had been
deleted out from under a running server).

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

### ☑ D1. Leptos shell + transport — **done**
`md-ui`, a Leptos CSR client built by `rust/build-ui.sh` into `rust/web/dist`
(478 KB wasm). Login, roster, conversation, composer; RPC over HTTP and pushes
over one reconnecting WebSocket. The session cookie stays HttpOnly and the
client never holds the token — a token in WASM memory is reachable from any
script on the page.

Not built inside the Docker image: trunk pulls a wasm toolchain and `wasm-opt`,
which would add minutes to every server rebuild for something that changes far
less often. The dev console remains at `/console.html`.

### ☑ D1b. Composer + conversation view — **done**, see task #16
The replacement for the terminal. Entries come from the agent's own session
transcript — real tool names, arguments and results — instead of from regexing
escape sequences off a screen. The composer types into the agent's PTY, because
that is how you talk to a CLI: the PTY is still the transport, it is simply
never rendered.

Reading is incremental. The client passes back the byte offset it last saw, so
following a long session stays cheap, and a read stops on the last newline —
a record still being written is left for the next call rather than half-parsed.

Verified against a REAL Claude Code transcript, not a synthetic one. Two things
that finding turned up:

- **Most records are not conversation.** A live file also carries `ai-title`,
  `agent-name`, `mode`, `permission-mode`, `queue-operation`,
  `file-history-snapshot`, `system`, `attachment` and `last-prompt` records,
  plus `isMeta` / `isVisibleInTranscriptOnly` messages. All correctly skipped:
  14 real records yielded exactly the 2 that were conversation.
- **Thinking text is never persisted.** All 116 thinking blocks in a real
  session file carry a signature and an empty string. So the "show thinking"
  control appears only when there is thinking to show — otherwise it would be a
  control that provably does nothing. The parser still handles it, for a CLI
  version that does persist it.

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
