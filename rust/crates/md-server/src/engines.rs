//! The agent CLIs a floor can hire, and what running one means.
//!
//! An agent is a process. Which process was, until now, a free-text box that
//! defaulted to `claude` and a hard-coded `provider == "claude"` check buried in
//! provisioning — so running anything else meant typing a binary name and
//! hoping, and adding a second supported CLI meant editing Rust.
//!
//! An **engine** is that decision as data: a name, a command, its arguments, and
//! the one thing the harness genuinely needs to know about a CLI —
//!
//! * `hooks` — whether it speaks Claude Code's hook and settings protocol. A
//!   hooked engine gets `--settings` and `--append-system-prompt`, and reports
//!   every tool call to the floor. A hookless one spawns bare and joins the hive
//!   through the terminal handoff instead: mail typed into its REPL. Both are
//!   citizens; only one of them narrates.
//!
//! Built-ins are a starting point, not a whitelist. A tenant registers its own
//! under `engines` in the config, overriding any field of a built-in or adding
//! something the catalogue has never heard of — which is the point, because the
//! set of coding CLIs changes faster than this file does.

use std::path::Path;

use serde_json::{json, Value};

/// One engine the harness ships knowing about.
pub struct Builtin {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// Speaks Claude Code's hook and settings protocol.
    pub hooks: bool,
    /// How to install it, if the catalogue knows. Run by an operator from the
    /// engines panel, never by an agent.
    ///
    /// Empty where the answer is not a one-liner or where guessing it would be
    /// worse than admitting ignorance — a package name invented here installs
    /// something, and what it installs is not necessarily this. Set your own
    /// under `engines.<id>.install`.
    pub install: &'static str,
}

/// The presets the Electron original shipped, plus a local-model runner and a
/// plain shell. `custom` is not among them: an engine with no command is not an
/// engine, and registering your own is what the panel is for.
///
/// Only `claude` is hooked, and that is a statement about this port rather than
/// about the CLIs: the original bridges Antigravity, Codex and Grok with
/// translating shims and Qwen with a reverse-proxy sidecar, and none of that is
/// ported. Claiming hooks an engine does not have would put an agent on the
/// floor that looks live and reports nothing, which is worse than admitting it
/// is quiet.
pub const CATALOG: &[Builtin] = &[
    Builtin {
        id: "claude", label: "Claude Code",
        description: "Anthropic's CLI. Reports every tool call, so the floor \
                      shows what it is doing.",
        command: "claude", args: &[], hooks: true, install: "npm install -g @anthropic-ai/claude-code",
    },
    Builtin {
        id: "codex", label: "Codex · GPT",
        description: "OpenAI's coding CLI. Runs, but is quiet on the floor.",
        command: "codex", args: &[], hooks: false, install: "npm install -g @openai/codex",
    },
    Builtin {
        id: "gemini", label: "Gemini CLI",
        description: "Google's coding CLI. Runs, but is quiet on the floor.",
        command: "gemini", args: &[], hooks: false, install: "npm install -g @google/gemini-cli",
    },
    Builtin {
        id: "antigravity", label: "Antigravity · Gemini",
        description: "Antigravity's agent CLI, on Gemini models. Quiet on the floor.",
        command: "agy", args: &[], hooks: false, install: "",
    },
    Builtin {
        id: "grok", label: "Grok · xAI",
        description: "xAI's coding CLI. Quiet on the floor.",
        command: "grok", args: &[], hooks: false, install: "",
    },
    Builtin {
        id: "kimi", label: "Kimi Code",
        description: "Moonshot's coding CLI. Quiet on the floor, and takes no \
                      inbox mail — work reaches it typed into its prompt.",
        command: "kimi", args: &[], hooks: false, install: "",
    },
    Builtin {
        id: "qwen", label: "Qwen Code",
        description: "Alibaba's coding CLI. Quiet on the floor.",
        command: "qwen", args: &[], hooks: false, install: "npm install -g @qwen-code/qwen-code",
    },
    Builtin {
        id: "opencode", label: "OpenCode",
        description: "The open-source terminal agent. Quiet on the floor.",
        command: "opencode", args: &[], hooks: false, install: "npm install -g opencode-ai",
    },
    Builtin {
        id: "crush", label: "Crush · Charm",
        description: "Charm's terminal agent. Quiet on the floor.",
        command: "crush", args: &[], hooks: false, install: "",
    },
    Builtin {
        id: "pi", label: "Pi",
        description: "The Pi coding CLI. Quiet on the floor.",
        command: "pi", args: &[], hooks: false, install: "",
    },
    Builtin {
        id: "copilot", label: "Copilot",
        description: "GitHub Copilot's CLI. Quiet on the floor, and takes no \
                      inbox mail — work reaches it typed into its prompt.",
        command: "copilot", args: &[], hooks: false, install: "npm install -g @github/copilot",
    },
    Builtin {
        id: "cursor", label: "Cursor",
        description: "Cursor's headless agent. Quiet on the floor.",
        command: "cursor-agent", args: &[], hooks: false, install: "",
    },
    Builtin {
        id: "ollama", label: "Ollama",
        description: "A model running on this machine. Set which one in the \
                      arguments.",
        command: "ollama", args: &["run", "llama3.2"], hooks: false, install: "",
    },
    Builtin {
        id: "shell", label: "Plain shell",
        description: "A bare shell, for looking around a workspace. Not an agent.",
        command: "bash", args: &[], hooks: false, install: "",
    },
];


/// One engine, built-in defaults merged with whatever the tenant changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Engine {
    pub id: String,
    pub label: String,
    pub description: String,
    pub command: String,
    pub args: Vec<String>,
    pub hooks: bool,
    pub install: String,
    /// Whether the catalogue ships it. A built-in can be edited and hidden but
    /// never deleted — the definition would come back on the next release and
    /// the "deletion" would look like a bug.
    pub builtin: bool,
}

fn strings(v: Option<&Value>) -> Option<Vec<String>> {
    Some(
        v?.as_array()?
            .iter()
            .filter_map(|a| a.as_str().map(String::from))
            .collect(),
    )
}

impl Engine {
    fn from_builtin(b: &Builtin) -> Self {
        Self {
            id: b.id.into(),
            label: b.label.into(),
            description: b.description.into(),
            command: b.command.into(),
            args: b.args.iter().map(|a| (*a).to_string()).collect(),
            hooks: b.hooks,
            install: b.install.into(),
            builtin: true,
        }
    }

    /// Apply a tenant's overrides. Field by field, so changing only the command
    /// keeps the label and the description that explain what it is.
    fn patch(&mut self, over: &Value) {
        if let Some(v) = over.get("label").and_then(|v| v.as_str()) {
            self.label = v.into();
        }
        if let Some(v) = over.get("description").and_then(|v| v.as_str()) {
            self.description = v.into();
        }
        if let Some(v) = over.get("command").and_then(|v| v.as_str()) {
            self.command = v.into();
        }
        if let Some(v) = strings(over.get("args")) {
            self.args = v;
        }
        if let Some(v) = over.get("hooks").and_then(|v| v.as_bool()) {
            self.hooks = v;
        }
        if let Some(v) = over.get("install").and_then(|v| v.as_str()) {
            self.install = v.into();
        }
    }
}

fn hidden(over: &Value) -> bool {
    over.get("hidden").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Every engine this tenant can hire, built-ins first and then its own.
pub fn all(config: &Value) -> Vec<Engine> {
    let over = config.get("engines").and_then(|v| v.as_object());

    let mut out: Vec<Engine> = CATALOG
        .iter()
        .filter_map(|b| {
            let patch = over.and_then(|o| o.get(b.id));
            if patch.is_some_and(hidden) {
                return None;
            }
            let mut e = Engine::from_builtin(b);
            if let Some(p) = patch {
                e.patch(p);
            }
            Some(e)
        })
        .collect();

    // Registered engines the catalogue has never heard of. A command is the one
    // thing that cannot be defaulted — an entry without one would be a name that
    // spawns nothing.
    if let Some(over) = over {
        for (id, v) in over {
            if CATALOG.iter().any(|b| b.id == id) || hidden(v) {
                continue;
            }
            let Some(command) = v.get("command").and_then(|c| c.as_str()).filter(|c| !c.trim().is_empty())
            else {
                continue;
            };
            out.push(Engine {
                id: id.clone(),
                label: v.get("label").and_then(|l| l.as_str()).unwrap_or(id).to_string(),
                description: v
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("Registered on this floor.")
                    .to_string(),
                command: command.to_string(),
                args: strings(v.get("args")).unwrap_or_default(),
                hooks: v.get("hooks").and_then(|h| h.as_bool()).unwrap_or(false),
                install: v.get("install").and_then(|i| i.as_str()).unwrap_or("").to_string(),
                builtin: false,
            });
        }
    }
    out
}

/// One engine by id.
pub fn resolve(config: &Value, id: &str) -> Option<Engine> {
    all(config).into_iter().find(|e| e.id == id)
}

/// Whether the command can actually be run.
///
/// Surfaced rather than enforced. A registered engine whose binary is not
/// installed is a normal state — you register it, then install it — and
/// refusing to save one would make the panel useless on a fresh machine. It is
/// still worth saying out loud, because "hired and immediately exited" is a
/// confusing way to learn that `gemini` is not on the PATH.
pub fn available(command: &str) -> bool {
    let cmd = command.trim();
    if cmd.is_empty() {
        return false;
    }
    // An explicit path is checked where it points; a bare name is looked up the
    // way a shell would.
    if cmd.contains(std::path::MAIN_SEPARATOR) || cmd.contains('/') {
        return Path::new(cmd).is_file();
    }
    // The AGENT's path, not the server's. They differ exactly where it matters:
    // a server started as a daemon has a stub PATH, and in a container the
    // engines are installed to a directory only `MD_AGENT_PATH` names. Asking
    // the server's own environment answers a question nobody asked.
    std::env::split_paths(md_pty::env::agent_path()).any(|dir| dir.join(cmd).is_file())
}

/// The catalogue as the setup panel shows it.
pub fn view(config: &Value) -> Value {
    json!(all(config)
        .into_iter()
        .map(|e| json!({
            "id": e.id,
            "label": e.label,
            "description": e.description,
            "command": e.command,
            "args": e.args,
            "hooks": e.hooks,
            "install": e.install,
            "builtin": e.builtin,
            "available": available(&e.command),
        }))
        .collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalogue_stands_on_its_own() {
        let list = all(&json!({}));
        assert!(list.iter().any(|e| e.id == "claude" && e.hooks));
        assert!(list.iter().all(|e| e.builtin));
        // Every built-in has something to run and something to read.
        for e in &list {
            assert!(!e.command.trim().is_empty(), "{} has no command", e.id);
            assert!(!e.label.is_empty() && !e.description.is_empty(), "{}", e.id);
        }
        // Exactly one hooked engine, and it says so: claiming hooks an engine
        // does not have puts an agent on the floor that reports nothing.
        assert_eq!(list.iter().filter(|e| e.hooks).count(), 1);
        // Every preset the original shipped, so switching floors does not mean
        // losing the CLI you were using.
        for id in ["claude", "codex", "grok", "kimi", "gemini", "antigravity",
                   "qwen", "opencode", "crush", "pi", "copilot", "cursor"] {
            assert!(list.iter().any(|e| e.id == id), "{id} is missing from the catalogue");
        }
        let mut ids: Vec<&str> = list.iter().map(|e| e.id.as_str()).collect();
        let n = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), n, "two engines share an id");
    }

    #[test]
    fn a_tenant_can_change_one_field_of_a_builtin() {
        let cfg = json!({ "engines": { "claude": { "args": ["--verbose"] } } });
        let e = resolve(&cfg, "claude").unwrap();
        assert_eq!(e.args, vec!["--verbose"]);
        // ...and keeps everything it did not touch.
        assert_eq!(e.command, "claude");
        assert_eq!(e.label, "Claude Code");
        assert!(e.hooks && e.builtin);
    }

    #[test]
    fn a_tenant_can_register_something_new() {
        let cfg = json!({ "engines": { "mine": {
            "label": "My Agent", "command": "my-agent", "args": ["--repl"], "hooks": true,
        }}});
        let e = resolve(&cfg, "mine").unwrap();
        assert_eq!((e.command.as_str(), e.builtin, e.hooks), ("my-agent", false, true));
        assert_eq!(e.label, "My Agent");
    }

    /// A name with nothing to run is not an engine. Saving one would put an
    /// entry in the picker that spawns nothing and reports no reason.
    #[test]
    fn a_registration_without_a_command_is_not_an_engine() {
        let cfg = json!({ "engines": { "empty": { "label": "Empty" } } });
        assert!(resolve(&cfg, "empty").is_none());
        let cfg = json!({ "engines": { "blank": { "command": "   " } } });
        assert!(resolve(&cfg, "blank").is_none());
    }

    #[test]
    fn a_builtin_can_be_hidden_but_the_rest_stay() {
        let cfg = json!({ "engines": { "shell": { "hidden": true } } });
        let list = all(&cfg);
        assert!(!list.iter().any(|e| e.id == "shell"));
        assert!(list.iter().any(|e| e.id == "claude"));
    }

    /// The catalogue only claims an installer where it knows the package. A
    /// guessed npm name installs SOMETHING, and what it installs is not
    /// necessarily the CLI you asked for.
    #[test]
    fn an_engine_offers_an_installer_only_where_one_is_known() {
        let list = all(&json!({}));
        let claude = list.iter().find(|e| e.id == "claude").unwrap();
        assert_eq!(claude.install, "npm install -g @anthropic-ai/claude-code");
        // A local model runner is not an npm package, and saying so is better
        // than shipping a command that fails in an unhelpful way.
        assert!(list.iter().find(|e| e.id == "ollama").unwrap().install.is_empty());
        assert!(list.iter().find(|e| e.id == "shell").unwrap().install.is_empty());
    }

    /// ...and a tenant can supply the recipe the catalogue lacks, for a built-in
    /// or for its own engine.
    #[test]
    fn a_tenant_can_teach_the_panel_how_to_install_something() {
        let cfg = json!({ "engines": {
            "ollama": { "install": "curl -fsSL https://ollama.com/install.sh | sh" },
            "mine": { "command": "my-agent", "install": "cargo install my-agent" },
        }});
        assert!(resolve(&cfg, "ollama").unwrap().install.starts_with("curl"));
        assert_eq!(resolve(&cfg, "mine").unwrap().install, "cargo install my-agent");
        // And the panel can see it, which is what decides whether a button shows.
        let v = view(&cfg);
        let row = v.as_array().unwrap().iter().find(|r| r["id"] == "mine").unwrap();
        assert_eq!(row["install"], "cargo install my-agent");
    }

    #[test]
    fn availability_finds_a_real_binary_and_misses_an_invented_one() {
        assert!(available("sh"), "sh is on the PATH of anything running this");
        assert!(!available("md-definitely-not-a-real-binary"));
        assert!(!available(""));
        assert!(!available("/nowhere/at/all/nope"));
    }

    #[test]
    fn the_view_says_whether_each_engine_can_actually_run() {
        let cfg = json!({ "engines": { "sh": { "command": "sh" } } });
        let v = view(&cfg);
        let row = v.as_array().unwrap().iter().find(|r| r["id"] == "sh").unwrap();
        assert_eq!(row["available"], true);
        assert_eq!(row["builtin"], false);
    }
}
