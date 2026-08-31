//! Agent provisioning: what has to exist on disk before an agent CLI starts,
//! and what has to be handed to it so it joins the hive.
//!
//! Which CLI an agent runs is an ENGINE, defined in `engines.rs` and chosen per
//! agent. Provisioning cares about exactly one thing from it: whether the CLI
//! speaks Claude Code's hook and settings protocol. A hooked engine is wired to
//! report; a hookless one spawns bare and reaches the hive through the terminal
//! handoff instead.
//!
//! The Electron original also bridges hookless CLIs (`agy`, `codex`, `pi`, and a
//! reverse-proxy sidecar for `qwen`), each with its own shim and config layout;
//! that is a separate surface and is not ported, which is why the catalogue
//! marks only Claude Code as hooked.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::hive::Hive;

/// Status-like captions that must never be stored as an agent's durable job.
/// A restart passes the floor roster's `description`, which may hold "on
/// standby" — writing that over the hire role would lose the real one.
const TRANSIENT_ROLE: [&str; 12] = [
    "standby", "on standby", "idle", "awaiting", "paused", "resumed", "working",
    "thinking", "archived", "starting up", "reconnecting", "running the floor",
];

/// What the caller must give us to provision an agent.
pub struct AgentMeta {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub role: Option<String>,
    pub cwd: String,
    pub is_god: bool,
    /// The character this agent is playing: a one-line description of how it
    /// behaves. Written into `identity.md` and the system prompt, so a recast
    /// changes what the agent IS, not only what it looks like.
    pub persona: Option<String>,
    /// The cast slot the operator picked at hire — `engineer`, `counsel` — and
    /// the two places on the map they posted it to.
    ///
    /// Durable, and `None` means "leave whatever is already recorded": a
    /// respawn carries no opinion about the floor, and letting it write one
    /// would silently un-post every agent on the next restart.
    pub archetype: Option<String>,
    pub primary_poi: Option<String>,
    pub secondary_poi: Option<String>,
    /// Whether this agent's CLI speaks Claude Code's hook and settings
    /// protocol.
    ///
    /// Resolved from the engine by the caller rather than inferred from the
    /// provider name here. `provider == "claude"` was true of the only engine
    /// that existed; the moment a tenant can register its own, the name says
    /// nothing about the protocol and a hard-coded comparison would silently
    /// deny hooks to a CLI that has them.
    pub hooks: bool,
    /// What was actually run, recorded so the roster can show it. Derived from
    /// the engine unless the caller overrode it.
    pub command: Option<String>,
}

/// What the PTY spawn needs to add so the agent is hive-aware.
#[derive(Debug)]
pub struct Injection {
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// A real hire role always beats a status-like caption. When both are durable
/// the incoming value wins — that is the one the operator just set.
fn preferred_role(candidate: Option<&str>, existing: Option<&str>, is_god: bool) -> String {
    let durable = |s: Option<&str>| -> Option<String> {
        let v = s?.trim();
        if v.is_empty() {
            return None;
        }
        let lower = v.trim_end_matches('…').trim_end_matches('.').to_lowercase();
        (!TRANSIENT_ROLE.contains(&lower.as_str())).then(|| v.to_string())
    };
    durable(candidate)
        .or_else(|| durable(existing))
        .or_else(|| candidate.map(str::trim).filter(|s| !s.is_empty()).map(String::from))
        .or_else(|| existing.map(str::trim).filter(|s| !s.is_empty()).map(String::from))
        .unwrap_or_else(|| if is_god { "orchestrator (god)".into() } else { "agent".into() })
}

/// A spawn needs an absolute path that exists as a directory. Surfaced on the
/// registry as `cwdValid` so a bad value is visible on the roster instead of
/// becoming a confusing spawn failure later.
fn cwd_validity(cwd: &str) -> (bool, Option<&'static str>) {
    if cwd.trim().is_empty() {
        return (false, Some("missing"));
    }
    let p = Path::new(cwd);
    if !p.is_absolute() {
        return (false, Some("not-absolute"));
    }
    if !p.is_dir() {
        return (false, Some("not-a-directory"));
    }
    (true, None)
}

pub struct Provisioner<'a> {
    pub hive: &'a Hive,
    pub hive_root: PathBuf,
    /// Absolute path to the `md-hook` shim inside the agent's environment.
    pub hook_bin: String,
    /// The tenant's config, which decides its MCP consent.
    pub config_file: PathBuf,
}

impl Provisioner<'_> {
    /// Create the hive's own files if they are missing.
    ///
    /// Seeded files are created only when absent — `board.md` and `tasks.json`
    /// hold real work, and rewriting them on every spawn would erase it.
    pub fn ensure_hive(&self) -> std::io::Result<()> {
        let root = &self.hive_root;
        std::fs::create_dir_all(root.join("agents"))?;

        let seed = |name: &str, contents: &str| -> std::io::Result<()> {
            let p = root.join(name);
            if !p.exists() {
                std::fs::write(p, contents)?;
            }
            Ok(())
        };
        seed("registry.json", "{\n  \"godId\": null,\n  \"agents\": {}\n}\n")?;
        seed("tasks.json", "{\n  \"tasks\": []\n}\n")?;
        seed("log.jsonl", "")?;
        seed(
            "board.md",
            "# Hive board\n\n_Shared plans live here. The god agent is the scribe._\n",
        )?;

        // Live, churny files that must never enter the hive's git history. The
        // socket in particular is not a file worth versioning and changes inode
        // on every restart.
        let ignore = root.join(".gitignore");
        let existing = std::fs::read_to_string(&ignore).unwrap_or_default();
        let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
        let mut added = false;
        for want in ["fleet.json", "hooks.sock", "cost-ledger.jsonl", ".DS_Store"] {
            if !lines.iter().any(|l| l == want) {
                lines.push(want.to_string());
                added = true;
            }
        }
        if added {
            std::fs::write(&ignore, lines.join("\n") + "\n")?;
        }
        Ok(())
    }

    /// Provision one agent and return what its spawn must add.
    ///
    /// Safe to call on every spawn: identity is refreshed, memory is seeded only
    /// once, and the registry entry is merged rather than replaced.
    pub fn ensure_agent(&self, meta: &AgentMeta) -> Result<Injection, String> {
        // Non-Claude providers spawn, but without hook wiring: their lifecycle
        // shims (agy, codex, pi) and the qwen proxy sidecar are not ported. They
        // are hive citizens through the TERMINAL HANDOFF instead — mail typed
        // into their REPL — which is the same path the original uses for a
        // hookless CLI. Refusing them outright, as this did, kept a working
        // delivery route unavailable for want of an unrelated bridge.
        let hooked = meta.hooks;
        if meta.id.trim().is_empty() {
            return Err("agent needs an id".into());
        }
        self.ensure_hive().map_err(|e| format!("hive bootstrap: {e}"))?;

        let dir = self.hive_root.join("agents").join(&meta.id);
        // `.done` and `.sent` are where the router archives handled mail; making
        // them now means a drain never has to create them mid-delivery.
        for sub in ["inbox/.done", "outbox/.sent"] {
            std::fs::create_dir_all(dir.join(sub)).map_err(|e| format!("agent dir: {e}"))?;
        }

        let reg = self.hive.registry();
        let prev = reg.get("agents").and_then(|a| a.get(&meta.id));
        let role = preferred_role(
            meta.role.as_deref(),
            prev.and_then(|p| p.get("role")).and_then(|v| v.as_str()),
            meta.is_god,
        );

        // Refreshed every spawn: it is generated from the registry, and a stale
        // copy would describe a job the agent no longer has.
        std::fs::write(dir.join("identity.md"), self.identity_text(meta, &role))
            .map_err(|e| format!("identity.md: {e}"))?;

        // Seeded once. This is the agent's durable memory — rewriting it on a
        // respawn would erase everything it had learned.
        let memory = dir.join("memory.md");
        if !memory.exists() {
            let _ = std::fs::write(
                &memory,
                format!(
                    "# Memory — {} ({})\n\n_Append durable facts, decisions, and context below._\n",
                    meta.name, meta.id
                ),
            );
        }
        let cursor = dir.join("cursor.json");
        if !cursor.exists() {
            let _ = std::fs::write(&cursor, "{\n  \"lastProcessed\": null\n}\n");
        }

        let (cwd_valid, issue) = cwd_validity(&meta.cwd);
        self.upsert_registry(meta, &role, cwd_valid, issue);

        // The settings file is what makes a Claude agent report to the floor: it
        // points every lifecycle hook at the shim, which relays to the socket.
        let settings = dir.join("settings.json");
        if hooked {
            // The tenant's own config decides which tool servers this agent gets.
            let config: Value = std::fs::read_to_string(self.config_file.as_path())
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_else(|| json!({}));
            std::fs::write(
                &settings,
                serde_json::to_vec_pretty(&self.hook_settings_with(&config, &meta.cwd))
                    .unwrap_or_default(),
            )
            .map_err(|e| format!("settings.json: {e}"))?;
        }

        let mut env = BTreeMap::new();
        env.insert("AGENT_ID".into(), meta.id.clone());
        env.insert("AGENT_NAME".into(), meta.name.clone());
        env.insert("HIVE_ROOT".into(), self.hive_root.display().to_string());
        env.insert("AGENT_DIR".into(), dir.display().to_string());
        // Read by `md-hook`. The shim is invoked by the CLI through `sh -c` with a
        // stripped environment, so the socket path has to travel in the agent's
        // own env rather than being discovered.
        env.insert(
            "MD_HOOK_SOCK".into(),
            self.hive_root.join("hooks.sock").display().to_string(),
        );

        // Claude takes its identity on a flag; a CLI that does not understand
        // Claude's flags gets nothing on argv, and receives the same identity
        // through the terminal handoff instead. Passing an unknown flag would
        // make the process exit before it started.
        let args = if hooked {
            vec![
                "--append-system-prompt".into(),
                self.injected_prompt(meta, &dir),
                // `--settings` rather than editing the user's own config: the
                // harness must never mutate files in the user's repo or home.
                "--settings".into(),
                settings.display().to_string(),
            ]
        } else {
            vec![]
        };

        Ok(Injection { args, env })
    }

    /// Merge the agent into the registry.
    ///
    /// The PRIOR entry is spread first so a respawn preserves fields the caller
    /// does not carry — above all `sessionId`. Losing it means `--resume` is
    /// never attached and every restart silently begins a fresh thread.
    fn upsert_registry(&self, meta: &AgentMeta, role: &str, cwd_valid: bool, issue: Option<&str>) {
        let mut reg = self.hive.registry();
        let agents = reg
            .get_mut("agents")
            .and_then(|a| a.as_object_mut())
            .expect("registry always has an agents object");

        let mut entry = agents
            .get(&meta.id)
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        for (k, v) in [
            ("id", json!(meta.id)),
            ("name", json!(meta.name)),
            ("provider", json!(meta.provider)),
            // What ran, so the roster can say so without re-resolving the
            // engine — and so an agent hired against an engine that was later
            // edited still shows what it actually started with.
            ("command", json!(meta.command.clone().unwrap_or_else(|| meta.provider.clone()))),
            ("role", json!(role)),
            ("cwd", json!(meta.cwd)),
            ("isGod", json!(meta.is_god)),
            ("status", json!("idle")),
            ("cwdValid", json!(cwd_valid)),
            // A (re)spawn always means a live terminal, so any prior archive is
            // stale by definition.
            ("archived", json!(false)),
            ("lastSeen", json!(now_ms())),
        ] {
            entry.insert(k.into(), v);
        }
        // Only when the caller has one. These are the operator's choices, not
        // the spawn's, and a restart must not overwrite them with nothing.
        for (k, v) in [
            ("archetype", &meta.archetype),
            ("primaryPoi", &meta.primary_poi),
            ("secondaryPoi", &meta.secondary_poi),
        ] {
            if let Some(v) = v.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                entry.insert(k.into(), json!(v));
            }
        }
        entry.entry("capabilities").or_insert_with(|| json!([]));
        agents.insert(meta.id.clone(), Value::Object(entry));

        if meta.is_god {
            reg["godId"] = json!(meta.id);
        }
        let _ = self.hive.write_registry(&reg);

        self.hive.log(json!({
            "kind": "spawn", "agentId": meta.id, "name": meta.name, "isGod": meta.is_god,
        }));
        // Only on the rare invalid case, so this is not a per-spawn log line.
        if !cwd_valid {
            self.hive.log(json!({
                "kind": "cwd_invalid", "agentId": meta.id, "cwd": meta.cwd, "issue": issue,
            }));
        }
    }

    /// The per-session settings that wire Claude Code's hooks to the shim.
    ///
    /// Written per agent and passed with `--settings`, never merged into the
    /// user's `~/.claude` — their own MCP servers and theme are not ours to
    /// touch.
    /// The per-session settings that wire Claude Code's hooks to the shim, plus
    /// the tenant's consented MCP servers for an agent working in `cwd`.
    fn hook_settings_with(&self, config: &Value, cwd: &str) -> Value {
        let entry = |matcher: Option<&str>| match matcher {
            Some(m) => json!({ "matcher": m, "hooks": [{ "type": "command", "command": self.hook_bin }] }),
            None => json!({ "hooks": [{ "type": "command", "command": self.hook_bin }] }),
        };
        let mut settings = json!({
            // 'auto', not a pinned light/dark. A pinned value matched the theme at
            // spawn and then ignored every change; 'auto' is the value that
            // listens for the terminal's own theme notifications.
            "theme": "auto",
            // The status line receives context_window accounting after every
            // response — the only clean programmatic source for the session's REAL
            // window size (200k vs 1M).
            "statusLine": {
                "type": "command",
                "command": format!("{} --status", self.hook_bin),
                "padding": 0,
            },
            "hooks": {
                "Stop": [entry(None)],
                "SubagentStop": [entry(None)],
                "PreToolUse": [entry(Some("*"))],
                "PostToolUse": [entry(Some("*"))],
                "UserPromptSubmit": [entry(None)],
                "Notification": [entry(None)],
                "SessionStart": [entry(None)],
                // Surfaces mid-compact so an agent boxing up its context reads as
                // busy on the floor instead of looking frozen.
                "PreCompact": [entry(None)],
                "PostCompact": [entry(None)],
            }
        });

        // The tenant's consented MCP servers, written into the PER-SESSION
        // settings only — never ~/.claude, so a user's own servers are never
        // clobbered. Omitted entirely when empty, so a file with nothing enabled
        // is unchanged from before.
        let mcp = crate::mcp::servers_for(cwd, config);
        if mcp.as_object().map(|m| !m.is_empty()).unwrap_or(false) {
            settings["mcpServers"] = mcp;
        }
        settings
    }

    fn identity_text(&self, meta: &AgentMeta, role: &str) -> String {
        let persona = match &meta.persona {
            Some(p) if !p.trim().is_empty() => format!("- **Character:** {p}\n"),
            _ => String::new(),
        };
        format!(
            "# {name} ({id})\n\n\
             {persona}\
             - **Role:** {role}\n\
             - **Working directory:** `{cwd}`\n\
             - **Hive root:** `{root}`\n\n\
             You are part of a hive. Your mailbox is `inbox/`; durable notes go in\n\
             `memory.md`. The shared board is `{root}/board.md` and the task ledger is\n\
             `{root}/tasks.json`.\n",
            name = meta.name,
            id = meta.id,
            role = role,
            cwd = meta.cwd,
            root = self.hive_root.display(),
            persona = persona,
        )
    }

    fn injected_prompt(&self, meta: &AgentMeta, dir: &Path) -> String {
        // The character is a manner, not a mandate: it colours how the agent
        // writes, and is explicitly subordinate to doing the work correctly.
        // Without that line a persona will happily be used as an excuse.
        let persona = match &meta.persona {
            Some(p) if !p.trim().is_empty() => format!(
                " You are playing {p}. Let that colour your tone — never your judgement \
                 or your diligence. Being in character is not a reason to do the work badly."
            ),
            _ => String::new(),
        };
        // Only the orchestrator is told it can ask for tools, because only the
        // orchestrator may: a worker that read this would spend a turn being
        // refused, and telling every agent about a door it cannot open is a
        // reliable way to have them all try it.
        let tools = if meta.is_god {
            " You can also ask the floor itself for a new tool: write a message to \
             `harness` with a `tool` object — {\"id\", \"label\", \"description\", \
             \"command\", \"args\"} — and it is registered SWITCHED OFF. It reaches \
             nobody until the person running this floor turns it on, so say plainly \
             what it is for and why, and carry on without it in the meantime."
        } else {
            ""
        };
        format!(
            "You are {name}, agent `{id}` on a Munder Difflin floor.{persona} Your identity is at \
             `{dir}/identity.md` and your durable memory at `{dir}/memory.md` — read both \
             before acting. Mail arrives as JSON files in `{dir}/inbox/`; move each to \
             `inbox/.done` once handled. To send mail, write a JSON message into \
             `{dir}/outbox/`. The shared board is `{root}/board.md`.{tools}",
            name = meta.name,
            id = meta.id,
            dir = dir.display(),
            root = self.hive_root.display(),
        )
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A counter, not a timestamp: tests run in parallel and two starting in the
    /// same millisecond would share a directory, so one test's registry would
    /// show up in another's assertions.
    fn setup() -> (PathBuf, Hive) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "md-spawn-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let hive = Hive::new(root.clone());
        (root, hive)
    }

    fn meta(id: &str) -> AgentMeta {
        AgentMeta {
            id: id.into(),
            name: "Jim".into(),
            provider: "claude".into(),
            role: Some("sales".into()),
            cwd: std::env::temp_dir().display().to_string(),
            is_god: false,
            persona: Some("Believes every rule is load-bearing.".into()),
            archetype: None,
            primary_poi: None,
            secondary_poi: None,
            hooks: true,
            command: Some("claude".into()),
        }
    }

    /// A hire's chosen personality and posting must outlive the process that
    /// made it. A respawn carries no opinion about the floor, so writing one
    /// would silently un-post every agent the first time anything restarted.
    /// The orchestrator is told it can ask for a tool, and told the answer is
    /// always "off until a person says otherwise" — an agent that expects the
    /// tool to work immediately will build on sand.
    #[test]
    fn only_the_orchestrator_is_told_it_can_ask_for_tools() {
        let (root, hive) = setup();
        let p = Provisioner { hive: &hive, hive_root: root.clone(), hook_bin: "md-hook".into(),
                                  config_file: std::env::temp_dir().join("no-config.json") };

        let mut god = meta("michael");
        god.is_god = true;
        let prompt = p.injected_prompt(&god, &root.join("agents/michael"));
        assert!(prompt.contains("harness"), "the orchestrator is told how to ask");
        assert!(prompt.contains("SWITCHED OFF"), "and told it will not be armed by asking");

        let worker = p.injected_prompt(&meta("dwight"), &root.join("agents/dwight"));
        assert!(!worker.contains("harness"), "a worker is not told about a door it cannot open");
    }

    #[test]
    fn a_chosen_personality_and_posting_survive_a_respawn() {
        let (root, hive) = setup();
        let p = Provisioner { hive: &hive, hive_root: root.clone(), hook_bin: "/usr/local/bin/md-hook".into(),
                                  config_file: std::env::temp_dir().join("no-config.json") };

        let mut hired = meta("kaylee");
        hired.archetype = Some("engineer".into());
        hired.primary_poi = Some("engine-room".into());
        hired.secondary_poi = Some("cargo-hold".into());
        p.ensure_agent(&hired).unwrap();

        let entry = |h: &Hive| h.registry()["agents"]["kaylee"].clone();
        let a = entry(&hive);
        assert_eq!(a["archetype"], "engineer");
        assert_eq!(a["primaryPoi"], "engine-room");
        assert_eq!(a["secondaryPoi"], "cargo-hold");

        // A restart: same agent, nothing said about the floor.
        p.ensure_agent(&meta("kaylee")).unwrap();
        let a = entry(&hive);
        assert_eq!(a["archetype"], "engineer", "a respawn un-posted the agent");
        assert_eq!(a["primaryPoi"], "engine-room");
        assert_eq!(a["secondaryPoi"], "cargo-hold");

        // A blank is a blank, not an erasure — the UI sends "" for "wherever
        // this character usually works".
        let mut blank = meta("kaylee");
        blank.primary_poi = Some("  ".into());
        p.ensure_agent(&blank).unwrap();
        assert_eq!(entry(&hive)["primaryPoi"], "engine-room");
    }

    #[test]
    fn provisioning_creates_the_agent_workspace() {
        let (root, hive) = setup();
        let p = Provisioner { hive: &hive, hive_root: root.clone(), hook_bin: "/usr/local/bin/md-hook".into(),
                                  config_file: std::env::temp_dir().join("no-config.json") };
        let inj = p.ensure_agent(&meta("jim")).unwrap();

        let dir = root.join("agents/jim");
        for f in ["identity.md", "memory.md", "cursor.json", "settings.json"] {
            assert!(dir.join(f).exists(), "{f} missing");
        }
        assert!(dir.join("inbox/.done").is_dir());
        assert!(dir.join("outbox/.sent").is_dir());

        assert_eq!(inj.env["AGENT_ID"], "jim");
        assert_eq!(inj.env["MD_HOOK_SOCK"], root.join("hooks.sock").display().to_string());
        assert!(inj.args.contains(&"--settings".to_string()));
        assert!(inj.args.contains(&"--append-system-prompt".to_string()));
    }

    /// Losing `sessionId` on a respawn means `--resume` is never attached, so
    /// every restart silently starts a fresh thread.
    #[test]
    fn a_respawn_preserves_the_session_id_and_unmodelled_fields() {
        let (root, hive) = setup();
        let p = Provisioner { hive: &hive, hive_root: root, hook_bin: "md-hook".into(),
                                  config_file: std::env::temp_dir().join("no-config.json") };
        p.ensure_agent(&meta("jim")).unwrap();

        hive.record_session("jim", "sess-abc");
        hive.set_archived("jim", true);
        hive.patch_agent_role("jim", "sales");

        p.ensure_agent(&meta("jim")).unwrap();
        let a = &hive.registry()["agents"]["jim"];
        assert_eq!(a["sessionId"], "sess-abc");
        // A respawn means a live terminal, so a prior archive is stale.
        assert_eq!(a["archived"], false);
    }

    /// A restart passes the roster caption, which may be a status string. The
    /// durable hire role has to win, or it is lost on the first restart.
    #[test]
    fn a_status_caption_never_overwrites_a_real_role() {
        assert_eq!(preferred_role(Some("on standby"), Some("sales lead"), false), "sales lead");
        assert_eq!(preferred_role(Some("idle"), Some("sales lead"), false), "sales lead");
        // Both durable → the operator's new value wins.
        assert_eq!(preferred_role(Some("ops"), Some("sales lead"), false), "ops");
        // Nothing durable anywhere → keep something rather than blanking it.
        assert_eq!(preferred_role(Some("idle"), None, false), "idle");
        assert_eq!(preferred_role(None, None, true), "orchestrator (god)");
        assert_eq!(preferred_role(None, None, false), "agent");
    }

    /// A recast has to change what the agent IS, not only what it is called —
    /// otherwise "recast" is a relabelling with extra steps.
    #[test]
    fn the_character_reaches_the_agents_own_identity() {
        let (root, hive) = setup();
        let p = Provisioner { hive: &hive, hive_root: root.clone(), hook_bin: "md-hook".into(),
                              config_file: std::env::temp_dir().join("no-config.json") };
        let inj = p.ensure_agent(&meta("jim")).unwrap();

        let identity = std::fs::read_to_string(root.join("agents/jim/identity.md")).unwrap();
        assert!(identity.contains("Believes every rule is load-bearing"), "{identity}");

        let prompt = inj.args.join(" ");
        assert!(prompt.contains("Believes every rule is load-bearing"));
        // And it is bounded: a persona must not become an excuse for bad work.
        assert!(prompt.contains("never your judgement"), "the persona is unbounded");
    }

    #[test]
    fn a_bad_cwd_is_recorded_rather_than_failing_the_spawn() {
        let (root, hive) = setup();
        let p = Provisioner { hive: &hive, hive_root: root, hook_bin: "md-hook".into(),
                                  config_file: std::env::temp_dir().join("no-config.json") };
        let mut m = meta("jim");
        m.cwd = "relative/path".into();

        assert!(p.ensure_agent(&m).is_ok(), "the roster should show the problem, not the spawn");
        assert_eq!(hive.registry()["agents"]["jim"]["cwdValid"], false);
        assert!(hive
            .log_tail(10)
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "cwd_invalid" && e["issue"] == "not-absolute"));
    }

    /// A CLI that does not understand Claude's flags must not receive them —
    /// the process would exit before it started. It is a hive citizen through
    /// the terminal handoff instead.
    #[test]
    fn a_non_claude_provider_spawns_without_claude_flags() {
        let (root, hive) = setup();
        let p = Provisioner { hive: &hive, hive_root: root.clone(), hook_bin: "md-hook".into(),
                                  config_file: std::env::temp_dir().join("no-config.json") };
        let mut m = meta("ryan");
        m.provider = "codex".into();
        m.command = Some("codex".into());
        // The ENGINE says it is hookless, not the name: a tenant may register
        // something called anything at all and declare that it speaks the
        // protocol, and a name comparison would have got that wrong.
        m.hooks = false;

        let inj = p.ensure_agent(&m).unwrap();
        assert!(inj.args.is_empty(), "no Claude flags: {:?}", inj.args);
        // Still provisioned: workspace, identity and registry entry all exist,
        // because the agent is on the floor either way.
        assert!(root.join("agents/ryan/identity.md").exists());
        assert!(root.join("agents/ryan/inbox").is_dir());
        assert_eq!(hive.registry()["agents"]["ryan"]["provider"], "codex");
        assert_eq!(hive.registry()["agents"]["ryan"]["command"], "codex");
        // But no hook settings, because nothing would read them.
        assert!(!root.join("agents/ryan/settings.json").exists());
        // The env still carries identity, which the handoff prompt refers to.
        assert_eq!(inj.env["AGENT_ID"], "ryan");
    }

    /// The board and the ledger hold real work; a second bootstrap must not
    /// erase it.
    #[test]
    fn bootstrapping_twice_does_not_clobber_real_work() {
        let (root, hive) = setup();
        let p = Provisioner { hive: &hive, hive_root: root.clone(), hook_bin: "md-hook".into(),
                                  config_file: std::env::temp_dir().join("no-config.json") };
        p.ensure_hive().unwrap();
        std::fs::write(root.join("board.md"), "# real plans").unwrap();
        hive.add_task(json!({ "id": "t1", "title": "real work" }));

        p.ensure_hive().unwrap();
        assert_eq!(std::fs::read_to_string(root.join("board.md")).unwrap(), "# real plans");
        assert_eq!(hive.tasks()["tasks"][0]["id"], "t1");
    }

    #[test]
    fn the_socket_is_kept_out_of_the_hive_git_history() {
        let (root, hive) = setup();
        let p = Provisioner { hive: &hive, hive_root: root.clone(), hook_bin: "md-hook".into(),
                                  config_file: std::env::temp_dir().join("no-config.json") };
        p.ensure_hive().unwrap();
        p.ensure_hive().unwrap(); // must not duplicate entries

        let ignore = std::fs::read_to_string(root.join(".gitignore")).unwrap();
        assert_eq!(ignore.matches("hooks.sock").count(), 1);
        assert!(ignore.contains("fleet.json"));
    }

    /// Every hook the floor reacts to must be wired, or an agent goes quiet in a
    /// way that looks like it stopped working.
    #[test]
    fn every_lifecycle_hook_points_at_the_shim() {
        let (root, hive) = setup();
        let p = Provisioner { hive: &hive, hive_root: root, hook_bin: "/usr/local/bin/md-hook".into(),
                                  config_file: std::env::temp_dir().join("no-config.json") };
        let s = p.hook_settings_with(&json!({}), "/w");

        for h in ["Stop", "SubagentStop", "PreToolUse", "PostToolUse", "UserPromptSubmit",
                  "Notification", "SessionStart", "PreCompact", "PostCompact"] {
            let cmd = &s["hooks"][h][0]["hooks"][0]["command"];
            assert_eq!(cmd, "/usr/local/bin/md-hook", "{h} is not wired");
        }
        assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], "*");
        assert_eq!(s["statusLine"]["command"], "/usr/local/bin/md-hook --status");
        assert_eq!(s["theme"], "auto", "a pinned theme stops tracking changes");
    }
}
