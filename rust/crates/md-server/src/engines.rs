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
//! An engine's environment is where a remote model server's address lives, and
//! therefore where its API key lives too. Values under a **secret-shaped name**
//! (`is_secret`) are not kept in the config with the rest: they go to the
//! encrypted store, the config keeps only the NAME under `envSecrets`, and no
//! response ever carries the value back — the panel can write a credential and
//! never read one. Catalogue defaults are exempt, because a default that ships
//! in this file is not a secret no matter what it is called.
//!
//! Built-ins are a starting point, not a whitelist. A tenant registers its own
//! under `engines` in the config, overriding any field of a built-in or adding
//! something the catalogue has never heard of — which is the point, because the
//! set of coding CLIs changes faster than this file does.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{json, Value};

/// What the panel shows in place of a credential, and what it sends back to
/// mean "leave this one alone". Bullets rather than a word, so it cannot
/// collide with a value somebody actually wants to store.
pub const MASK: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";

/// Whether an environment variable's NAME says its value is a credential.
///
/// A heuristic, and deliberately one: the alternative is asking an operator to
/// tick "this is a secret" beside every line, which gets it wrong in the
/// direction that matters. The escape hatch is the name — a value you want to
/// be able to read back does not go in a variable called `..._TOKEN`.
pub fn is_secret(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"]
        .iter()
        .any(|needle| k.contains(needle))
}

/// The handle a credential is stored under. Scoped by engine, so two engines
/// pointed at two servers can hold two different `OPENAI_API_KEY`s.
pub fn secret_ref(engine: &str, key: &str) -> String {
    format!("engine:{engine}:{key}")
}

/// A value the panel sent back untouched: all bullets, meaning "keep it".
fn masked(v: &str) -> bool {
    let v = v.trim();
    !v.is_empty() && v.chars().all(|c| c == '\u{2022}')
}

/// What one panel save means for an engine's environment.
///
/// Pure, and separate from anything that writes: which values are credentials,
/// which are being replaced and which are being forgotten is the whole of the
/// decision, and it is the part worth testing without a key or a disk.
#[derive(Debug, Default, PartialEq)]
pub struct EnvPlan {
    /// Stays in the config, in the clear.
    pub plain: BTreeMap<String, String>,
    /// Goes to the encrypted store, keyed by environment name.
    pub store: BTreeMap<String, String>,
    /// Already stored, sent back masked: left exactly as it is.
    pub keep: Vec<String>,
    /// Stored, and now gone from the panel: dropped from the store.
    pub forget: Vec<String>,
}

impl EnvPlan {
    /// Every credential this engine will have after the save. What goes into
    /// the config under `envSecrets`, so a spawn knows what to look up.
    pub fn secret_names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.store.keys().cloned().chain(self.keep.iter().cloned()).collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Secret-shaped values sitting in a tenant's OWN override, in the clear.
///
/// These are the ones entered before there was anywhere better to put them.
/// Catalogue defaults are excluded on purpose: a placeholder printed in this
/// file is not a credential, whatever it is called.
pub fn plaintext_secrets(config: &Value, id: &str) -> BTreeMap<String, String> {
    config
        .get("engines")
        .and_then(|v| v.get(id))
        .and_then(|p| p.get("env"))
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter(|(k, _)| is_secret(k))
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Sort a submitted environment into the four cases, given the credentials
/// already held for this engine (`held`) and any still sitting in the config in
/// the clear (`plaintext`).
pub fn plan_env(submitted: &Value, held: &[String], plaintext: &BTreeMap<String, String>) -> EnvPlan {
    let mut plan = EnvPlan::default();
    let rows: Vec<(String, String)> = submitted
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    for (k, v) in rows {
        if !is_secret(&k) {
            plan.plain.insert(k, v);
            continue;
        }
        if masked(&v) {
            if held.contains(&k) {
                plan.keep.push(k);
            } else if let Some(old) = plaintext.get(&k) {
                // A credential that predates the store, sent back masked
                // because the panel could not show it either. Saving the engine
                // MOVES it rather than asking the operator to find it and type
                // it again — and rather than dropping it, which is what
                // treating an unrecognised mask as "nothing" would do.
                plan.store.insert(k, old.clone());
            }
            // A mask against a name we hold nothing for at all records nothing:
            // naming a credential that does not exist would make every spawn on
            // this engine refuse.
        } else if v.trim().is_empty() {
            // Cleared on purpose.
            if held.contains(&k) {
                plan.forget.push(k);
            }
        } else {
            plan.store.insert(k, v);
        }
    }

    // A line deleted from the panel is a credential deleted from the store —
    // that is the only delete gesture the textarea has.
    for k in held {

        if !plan.store.contains_key(k) && !plan.keep.contains(k) && !plan.forget.contains(k) {
            plan.forget.push(k.clone());
        }
    }
    plan.keep.sort();
    plan.forget.sort();
    plan
}

/// One engine the harness ships knowing about.
pub struct Builtin {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// Speaks Claude Code's hook and settings protocol.
    pub hooks: bool,
    /// Environment every agent on this engine gets.
    ///
    /// What makes a REMOTE model server expressible at all: an OpenAI-wire CLI
    /// is pointed at one by its base URL, and until now an engine was a command
    /// and nothing else. Values are overridable per floor, because the whole
    /// point of a base URL is that it is yours.
    pub env: &'static [(&'static str, &'static str)],
    /// How a model is chosen: a flag on the command line, an environment
    /// variable, or neither.
    ///
    /// Both exist because CLIs are split on it — Claude Code takes `--model`,
    /// while anything on the OpenAI wire reads `OPENAI_MODEL`. An engine that
    /// names neither has no model to pick and gets no picker.
    pub model_flag: &'static str,
    pub model_env: &'static str,
    /// Models the catalogue knows. A starting list, not a limit: the panel
    /// takes a typed-in name, and an engine backed by a server is asked what it
    /// is actually serving.
    pub models: &'static [&'static str],
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
        command: "claude", args: &[], hooks: true,
        install: "npm install -g @anthropic-ai/claude-code", env: &[],
        model_flag: "--model", model_env: "",
        // Aliases first: they keep pointing at the current model of that tier,
        // where a pinned id goes stale on the next release.
        models: &["opus", "sonnet", "haiku",
                  "claude-opus-5", "claude-sonnet-5", "claude-haiku-4-5"],
    },
    Builtin {
        id: "codex", label: "Codex · GPT",
        description: "OpenAI's coding CLI. Runs, but is quiet on the floor.",
        command: "codex", args: &[], hooks: false, install: "npm install -g @openai/codex", env: &[], model_flag: "", model_env: "", models: &[],
    },
    Builtin {
        id: "gemini", label: "Gemini CLI",
        description: "Google's coding CLI. Runs, but is quiet on the floor.",
        command: "gemini", args: &[], hooks: false, install: "npm install -g @google/gemini-cli", env: &[], model_flag: "", model_env: "", models: &[],
    },
    Builtin {
        id: "antigravity", label: "Antigravity · Gemini",
        description: "Antigravity's agent CLI, on Gemini models. Quiet on the floor.",
        command: "agy", args: &[], hooks: false, install: "", env: &[], model_flag: "", model_env: "", models: &[],
    },
    Builtin {
        id: "grok", label: "Grok · xAI",
        description: "xAI's coding CLI. Quiet on the floor.",
        command: "grok", args: &[], hooks: false, install: "", env: &[], model_flag: "", model_env: "", models: &[],
    },
    Builtin {
        id: "kimi", label: "Kimi Code",
        description: "Moonshot's coding CLI. Quiet on the floor, and takes no \
                      inbox mail — work reaches it typed into its prompt.",
        command: "kimi", args: &[], hooks: false, install: "", env: &[], model_flag: "", model_env: "", models: &[],
    },
    Builtin {
        id: "qwen", label: "Qwen Code",
        description: "Alibaba's coding CLI. Quiet on the floor.",
        command: "qwen", args: &[], hooks: false, install: "npm install -g @qwen-code/qwen-code", env: &[], model_flag: "", model_env: "", models: &[],
    },
    Builtin {
        id: "opencode", label: "OpenCode",
        description: "The open-source terminal agent. Quiet on the floor.",
        command: "opencode", args: &[], hooks: false, install: "npm install -g opencode-ai", env: &[], model_flag: "", model_env: "", models: &[],
    },
    Builtin {
        id: "crush", label: "Crush · Charm",
        description: "Charm's terminal agent. Quiet on the floor.",
        command: "crush", args: &[], hooks: false, install: "", env: &[], model_flag: "", model_env: "", models: &[],
    },
    Builtin {
        id: "pi", label: "Pi",
        description: "The Pi coding CLI. Quiet on the floor.",
        command: "pi", args: &[], hooks: false, install: "", env: &[], model_flag: "", model_env: "", models: &[],
    },
    Builtin {
        id: "copilot", label: "Copilot",
        description: "GitHub Copilot's CLI. Quiet on the floor, and takes no \
                      inbox mail — work reaches it typed into its prompt.",
        command: "copilot", args: &[], hooks: false, install: "npm install -g @github/copilot", env: &[], model_flag: "", model_env: "", models: &[],
    },
    Builtin {
        id: "cursor", label: "Cursor",
        description: "Cursor's headless agent. Quiet on the floor.",
        command: "cursor-agent", args: &[], hooks: false, install: "", env: &[], model_flag: "", model_env: "", models: &[],
    },
    Builtin {
        id: "ollama", label: "Ollama",
        description: "A model running on this machine. Set which one in the \
                      arguments.",
        command: "ollama", args: &["run", "llama3.2"], hooks: false, install: "", env: &[], model_flag: "", model_env: "", models: &[],
    },
    Builtin {
        id: "lmstudio", label: "LM Studio (remote)",
        description: "A model served by LM Studio on another machine. LM Studio is \
                      not an agent — this runs Qwen Code against its OpenAI-compatible \
                      endpoint, so set the address and model below to your server's.",
        // LM Studio speaks the OpenAI wire, so the engine is an OpenAI-wire CLI
        // pointed at it. Qwen Code is the one in this catalogue that takes its
        // endpoint from the environment; OpenCode and Crush want a config file
        // written per agent, which is a bigger surface and a version-specific
        // one.
        command: "qwen", args: &[], hooks: false,
        install: "npm install -g @qwen-code/qwen-code",
        // The defaults are LM Studio's own, with the host left as localhost —
        // a placeholder address would be a broken engine, and this one at least
        // works if you happen to be running it alongside the server.
        env: &[
            ("OPENAI_BASE_URL", "http://localhost:1234/v1"),
            // LM Studio ignores the key but the client insists on one.
            ("OPENAI_API_KEY", "lm-studio"),
            ("OPENAI_MODEL", "local-model"),
        ],
        // The model list comes from the server itself — LM Studio serves
        // whatever is loaded, which is not something a catalogue can know.
        model_flag: "", model_env: "OPENAI_MODEL", models: &[],
    },
    Builtin {
        id: "shell", label: "Plain shell",
        description: "A bare shell, for looking around a workspace. Not an agent.",
        command: "bash", args: &[], hooks: false, install: "", env: &[], model_flag: "", model_env: "", models: &[],
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
    pub env: BTreeMap<String, String>,
    /// Environment names whose values live in the encrypted store, not here.
    /// Names only — this struct never carries a credential either.
    pub env_secrets: Vec<String>,
    pub model_flag: String,
    pub model_env: String,
    pub models: Vec<String>,
    /// The chosen one. Empty means the CLI's own default.
    pub model: String,
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
            env: b.env.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect(),
            // The catalogue ships no credentials, so there is nothing to look
            // up until an operator enters one.
            env_secrets: Vec::new(),
            model_flag: b.model_flag.into(),
            model_env: b.model_env.into(),
            models: b.models.iter().map(|m| (*m).to_string()).collect(),
            model: String::new(),
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
        if let Some(v) = over.get("modelFlag").and_then(|v| v.as_str()) {
            self.model_flag = v.into();
        }
        if let Some(v) = over.get("modelEnv").and_then(|v| v.as_str()) {
            self.model_env = v.into();
        }
        if let Some(v) = strings(over.get("models")) {
            self.models = v;
        }
        if let Some(v) = over.get("model").and_then(|v| v.as_str()) {
            self.model = v.into();
        }
        // Names of credentials in the store. Replaced rather than merged: the
        // list IS what a save decided, and a name left behind would send the
        // spawn looking for something nobody holds.
        if let Some(v) = strings(over.get("envSecrets")) {
            self.env_secrets = v;
        }
        // MERGED, not replaced: overriding the base URL of a built-in should
        // not silently drop the API key that goes with it.
        if let Some(m) = over.get("env").and_then(|v| v.as_object()) {
            for (k, v) in m {
                match v.as_str() {
                    Some(s) => { self.env.insert(k.clone(), s.to_string()); }
                    // An explicit null removes one the built-in set.
                    None if v.is_null() => { self.env.remove(k); }
                    None => {}
                }
            }
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
                env: v.get("env").and_then(|e| e.as_object()).map(|m| {
                    m.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string()))).collect()
                }).unwrap_or_default(),
                env_secrets: strings(v.get("envSecrets")).unwrap_or_default(),
                model_flag: v.get("modelFlag").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                model_env: v.get("modelEnv").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                models: strings(v.get("models")).unwrap_or_default(),
                model: v.get("model").and_then(|x| x.as_str()).unwrap_or("").to_string(),
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

/// The engine's credentials, decrypted, ready to join a process environment.
///
/// Fails rather than spawning without one. An agent started with no API key
/// does not fail visibly — it fails as a refusal from a server, minutes later,
/// in a terminal nobody is watching — so a store that cannot produce what the
/// config says it holds stops the spawn instead.
pub fn secret_env(
    e: &Engine,
    store: &crate::secrets::Secrets,
) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for key in &e.env_secrets {
        match store.get(&secret_ref(&e.id, key)) {
            Ok(Some(v)) => out.push((key.clone(), v)),
            Ok(None) => {
                return Err(format!(
                    "{} expects {key} from the secret store and there is nothing there. \
                     Enter it again under setup \u{2192} engines.",
                    e.label
                ))
            }
            Err(err) => return Err(format!("{}: {key} could not be read \u{2014} {err}", e.label)),
        }
    }
    Ok(out)
}

/// The catalogue as the setup panel shows it.
///
/// The one thing this does not contain is a credential. `env` carries the
/// values that are safe to read back; `envSecrets` and `envPlaintext` carry the
/// NAMES of the ones that are not, which is enough for the panel to draw a
/// masked line and to say which of them is still sitting in the config in the
/// clear.
pub fn view(config: &Value) -> Value {
    json!(all(config)
        .into_iter()
        .map(|e| {
            // Only what an OPERATOR typed can be an exposed credential. A
            // catalogue default under a secret-shaped name is printed in this
            // file already, so masking it would cost a line of the panel and
            // protect nothing.
            let plaintext: Vec<String> =
                plaintext_secrets(config, &e.id).into_keys().collect();
            let shown: BTreeMap<String, String> = e
                .env
                .iter()
                .filter(|(k, _)| !plaintext.contains(k) && !e.env_secrets.contains(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            json!({
            "id": e.id,
            "label": e.label,
            "description": e.description,
            "command": e.command,
            "args": e.args,
            "hooks": e.hooks,
            "install": e.install,
            "env": shown,
            // Held encrypted: the panel shows these masked.
            "envSecrets": e.env_secrets,
            // Secret-shaped and still in the config in the clear, because they
            // were entered before there was anywhere better to put them.
            // Saving the engine moves them into the store.
            "envPlaintext": plaintext,
            "modelFlag": e.model_flag,
            "modelEnv": e.model_env,
            "models": e.models,
            "model": e.model,
            // Whether this engine has a model to choose at all — the panel
            // shows a picker on exactly these.
            "picksModel": !(e.model_flag.is_empty() && e.model_env.is_empty()),
            "builtin": e.builtin,
            "available": available(&e.command),
            })
        })
        .collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- credentials --------------------------------------------------------

    #[test]
    fn a_name_decides_whether_a_value_is_a_credential() {
        for k in ["OPENAI_API_KEY", "ANTHROPIC_AUTH_TOKEN", "GH_TOKEN",
                  "MY_SECRET", "DB_PASSWORD", "AWS_CREDENTIAL_FILE",
                  // Case is not the operator's problem.
                  "openai_api_key"] {
            assert!(is_secret(k), "{k} should be treated as a credential");
        }
        // The things an engine's environment is mostly made of.
        for k in ["OPENAI_BASE_URL", "OPENAI_MODEL", "PATH", "HOME", "TERM",
                  "MD_AGENT_PATH", "NO_COLOR"] {
            assert!(!is_secret(k), "{k} is not a credential");
        }
    }

    /// The whole point: a value typed under a secret-shaped name never reaches
    /// the config, and the catalogue never hands one back.
    #[test]
    fn a_credential_leaves_the_environment_and_only_its_name_remains() {
        let plan = plan_env(
            &json!({ "OPENAI_BASE_URL": "http://192.168.99.8:1234/v1",
                     "OPENAI_API_KEY": "sk-live-do-not-log-me" }),
            &[],
            &BTreeMap::new(),
        );
        assert_eq!(plan.plain.len(), 1, "only the address stays: {:?}", plan.plain);
        assert_eq!(plan.plain["OPENAI_BASE_URL"], "http://192.168.99.8:1234/v1");
        assert_eq!(plan.store["OPENAI_API_KEY"], "sk-live-do-not-log-me");
        assert_eq!(plan.secret_names(), vec!["OPENAI_API_KEY"]);

        // And the view built from that config cannot produce it.
        let cfg = json!({ "engines": { "lmstudio": {
            "env": plan.plain, "envSecrets": plan.secret_names() } } });
        let shown = view(&cfg).as_array().unwrap().iter()
            .find(|e| e["id"] == "lmstudio").cloned().unwrap();
        assert!(shown["env"].get("OPENAI_API_KEY").is_none(), "{}", shown["env"]);
        assert_eq!(shown["envSecrets"], json!(["OPENAI_API_KEY"]));
        assert!(!view(&cfg).to_string().contains("sk-live"), "no response may carry it");
    }

    /// The panel shows a mask, so it sends one back. That has to mean "leave it
    /// alone" — anything else and every unrelated save wipes the key.
    #[test]
    fn a_masked_value_sent_back_keeps_the_stored_one() {
        let held = vec!["OPENAI_API_KEY".to_string()];
        let plan = plan_env(&json!({ "OPENAI_API_KEY": MASK, "OPENAI_MODEL": "gemma" }), &held, &BTreeMap::new());
        assert!(plan.store.is_empty(), "nothing to re-encrypt");
        assert!(plan.forget.is_empty(), "and nothing to lose");
        assert_eq!(plan.keep, vec!["OPENAI_API_KEY"]);
        assert_eq!(plan.secret_names(), vec!["OPENAI_API_KEY"]);
    }

    /// A mask against a name nothing is held for would record a credential that
    /// does not exist, and every spawn on that engine would then refuse.
    #[test]
    fn a_mask_with_nothing_behind_it_records_nothing() {
        let plan = plan_env(&json!({ "GH_TOKEN": MASK }), &[], &BTreeMap::new());
        assert!(plan.secret_names().is_empty());
        assert!(plan.plain.is_empty(), "and it is certainly not kept in the clear");
    }

    #[test]
    fn clearing_or_deleting_a_line_forgets_the_credential() {
        let held = vec!["A_TOKEN".to_string(), "B_TOKEN".to_string()];
        // A emptied, B's line deleted outright.
        let plan = plan_env(&json!({ "A_TOKEN": "  " }), &held, &BTreeMap::new());
        assert_eq!(plan.forget, vec!["A_TOKEN", "B_TOKEN"]);
        assert!(plan.secret_names().is_empty());
    }

    #[test]
    fn replacing_a_stored_credential_re_encrypts_it_and_forgets_nothing() {
        let plan = plan_env(&json!({ "GH_TOKEN": "ghp_new" }), &["GH_TOKEN".to_string()], &BTreeMap::new());
        assert_eq!(plan.store["GH_TOKEN"], "ghp_new");
        assert!(plan.forget.is_empty(), "it is being replaced, not dropped");
    }

    /// A catalogue default under a secret-shaped name is printed in this file,
    /// so masking it costs a line of the panel and protects nothing. LM Studio
    /// ships `OPENAI_API_KEY=lm-studio` precisely because it is not a secret.
    #[test]
    fn a_catalogue_placeholder_stays_visible() {
        let shown = view(&json!({})).as_array().unwrap().iter()
            .find(|e| e["id"] == "lmstudio").cloned().unwrap();
        assert_eq!(shown["env"]["OPENAI_API_KEY"], "lm-studio");
        assert_eq!(shown["envSecrets"], json!([]));
        assert_eq!(shown["envPlaintext"], json!([]));
    }

    /// A key entered before there was anywhere better to put it. It still has
    /// to WORK — so it stays in the environment the spawn uses — while the
    /// panel says out loud that it is in the clear.
    #[test]
    fn a_credential_already_in_the_config_is_reported_rather_than_hidden() {
        let cfg = json!({ "engines": { "lmstudio": { "env": { "OPENAI_API_KEY": "sk-old" } } } });
        let shown = view(&cfg).as_array().unwrap().iter()
            .find(|e| e["id"] == "lmstudio").cloned().unwrap();
        assert_eq!(shown["envPlaintext"], json!(["OPENAI_API_KEY"]));
        assert!(shown["env"].get("OPENAI_API_KEY").is_none(), "not shown either way");
        assert!(!view(&cfg).to_string().contains("sk-old"));
        // ...and an agent hired on it still gets the key.
        assert_eq!(resolve(&cfg, "lmstudio").unwrap().env["OPENAI_API_KEY"], "sk-old");
    }

    /// The panel cannot show a plaintext key either, so it sends back a mask —
    /// and a mask it does not recognise must not become a deletion. Saving the
    /// engine moves the old value into the store instead, without anybody
    /// having to go and find it.
    #[test]
    fn saving_an_engine_migrates_a_plaintext_credential_rather_than_dropping_it() {
        let cfg = json!({ "engines": { "lmstudio": { "env": {
            "OPENAI_BASE_URL": "http://192.168.99.8:1234/v1",
            "OPENAI_API_KEY": "sk-old-and-exposed" } } } });
        let held = resolve(&cfg, "lmstudio").unwrap().env_secrets;
        assert!(held.is_empty(), "nothing is in the store yet");

        let plan = plan_env(
            &json!({ "OPENAI_BASE_URL": "http://192.168.99.8:1234/v1",
                     "OPENAI_API_KEY": MASK }),
            &held,
            &plaintext_secrets(&cfg, "lmstudio"),
        );
        assert_eq!(plan.store["OPENAI_API_KEY"], "sk-old-and-exposed", "moved, not lost");
        assert!(plan.forget.is_empty());
        assert!(!plan.plain.contains_key("OPENAI_API_KEY"), "and gone from the config");
        assert_eq!(plan.plain["OPENAI_BASE_URL"], "http://192.168.99.8:1234/v1");
    }

    /// Clearing the line is still a deletion, even for one that was never in
    /// the store.
    #[test]
    fn clearing_a_plaintext_credential_drops_it_from_the_config() {
        let cfg = json!({ "engines": { "x": { "command": "x", "env": { "GH_TOKEN": "ghp_old" } } } });
        let plan = plan_env(&json!({ "GH_TOKEN": "" }), &[], &plaintext_secrets(&cfg, "x"));
        assert!(plan.store.is_empty() && plan.plain.is_empty());
        assert!(plan.secret_names().is_empty());
    }

    #[test]
    fn a_spawn_refuses_when_the_store_cannot_produce_what_the_config_names() {
        let _g = crate::secrets::env_guard();
        std::env::set_var("MD_SECRET_KEY", "z".repeat(40));
        let dir = std::env::temp_dir().join(format!("md-eng-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = crate::secrets::Secrets::new(&dir);

        let cfg = json!({ "engines": { "lmstudio": { "envSecrets": ["OPENAI_API_KEY"] } } });
        let e = resolve(&cfg, "lmstudio").unwrap();
        // Nothing stored yet: an error, not an empty value.
        let err = secret_env(&e, &store).unwrap_err();
        assert!(err.contains("OPENAI_API_KEY"), "{err}");

        store.set(&secret_ref("lmstudio", "OPENAI_API_KEY"), "sk-real").unwrap();
        assert_eq!(
            secret_env(&e, &store).unwrap(),
            vec![("OPENAI_API_KEY".to_string(), "sk-real".to_string())]
        );
        std::env::remove_var("MD_SECRET_KEY");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An engine holding no credentials must not need the store at all — most
    /// of them never will, and a floor with no `MD_SECRET_KEY` still has to run.
    #[test]
    fn an_engine_with_no_credentials_asks_the_store_nothing() {
        let _g = crate::secrets::env_guard();
        std::env::remove_var("MD_SECRET_KEY");
        let e = resolve(&json!({}), "claude").unwrap();
        assert!(e.env_secrets.is_empty());
        let store = crate::secrets::Secrets::new(std::path::Path::new("/nonexistent"));
        assert_eq!(secret_env(&e, &store), Ok(vec![]));
    }

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

    /// LM Studio is a SERVER, not an agent, so the engine is an OpenAI-wire CLI
    /// aimed at it. What makes it usable is that the address is data.
    #[test]
    fn the_lm_studio_engine_points_a_cli_at_an_endpoint_you_can_move() {
        let e = resolve(&json!({}), "lmstudio").unwrap();
        assert_eq!(e.command, "qwen", "LM Studio does not run agents; something else does");
        assert_eq!(e.env["OPENAI_BASE_URL"], "http://localhost:1234/v1");
        assert!(!e.hooks, "nothing but Claude Code reports to the floor");

        // Pointing it at another machine is one field, and the key and model
        // that came with it survive.
        let cfg = json!({ "engines": { "lmstudio": {
            "env": { "OPENAI_BASE_URL": "http://192.168.1.50:1234/v1" }
        }}});
        let e = resolve(&cfg, "lmstudio").unwrap();
        assert_eq!(e.env["OPENAI_BASE_URL"], "http://192.168.1.50:1234/v1");
        assert_eq!(e.env["OPENAI_API_KEY"], "lm-studio", "a merge, not a replacement");
        assert_eq!(e.env["OPENAI_MODEL"], "local-model");
    }

    /// An override merges; only an explicit null removes. Replacing the map
    /// wholesale would mean changing an address silently dropped the key that
    /// went with it, and the failure would look like the server being down.
    #[test]
    fn engine_environment_merges_and_a_null_removes() {
        let cfg = json!({ "engines": { "lmstudio": {
            "env": { "OPENAI_MODEL": null, "EXTRA": "1" }
        }}});
        let e = resolve(&cfg, "lmstudio").unwrap();
        assert!(!e.env.contains_key("OPENAI_MODEL"));
        assert_eq!(e.env["EXTRA"], "1");
        assert_eq!(e.env["OPENAI_BASE_URL"], "http://localhost:1234/v1");
    }

    /// A registered engine gets an environment too — the same mechanism, so
    /// pointing your own CLI at your own server needs no new concept.
    #[test]
    fn a_registered_engine_can_carry_its_own_environment() {
        let cfg = json!({ "engines": { "mine": {
            "command": "my-agent", "env": { "MY_BASE_URL": "http://box:8080" }
        }}});
        assert_eq!(resolve(&cfg, "mine").unwrap().env["MY_BASE_URL"], "http://box:8080");
    }

    /// Everything else carries none, so adding the field changed no existing
    /// engine's behaviour.
    #[test]
    fn engines_that_need_no_environment_have_none() {
        for id in ["claude", "codex", "gemini", "shell"] {
            assert!(resolve(&json!({}), id).unwrap().env.is_empty(), "{id}");
        }
    }

    /// A model is passed the way its CLI takes it — a flag for Claude Code, an
    /// environment variable for anything on the OpenAI wire. An engine that
    /// names neither has nothing to pick.
    #[test]
    fn an_engine_says_how_it_takes_a_model() {
        let claude = resolve(&json!({}), "claude").unwrap();
        assert_eq!(claude.model_flag, "--model");
        assert!(claude.model_env.is_empty());
        // Aliases first: they track the current model of each tier, where a
        // pinned id goes stale on the next release.
        assert_eq!(claude.models.first().map(String::as_str), Some("opus"));
        assert!(claude.models.iter().any(|m| m == "claude-opus-5"));

        let lm = resolve(&json!({}), "lmstudio").unwrap();
        assert_eq!(lm.model_env, "OPENAI_MODEL");
        assert!(lm.model_flag.is_empty());
        // Nothing shipped: LM Studio serves whatever was loaded into it, which
        // is a question for the server, not for this file.
        assert!(lm.models.is_empty());

        assert!(resolve(&json!({}), "shell").unwrap().model_flag.is_empty());
    }

    #[test]
    fn a_tenant_can_choose_a_model_and_supply_its_own_list() {
        let cfg = json!({ "engines": {
            "claude": { "model": "sonnet" },
            "lmstudio": { "models": ["qwen2.5-coder-7b"], "model": "qwen2.5-coder-7b" },
        }});
        assert_eq!(resolve(&cfg, "claude").unwrap().model, "sonnet");
        let lm = resolve(&cfg, "lmstudio").unwrap();
        assert_eq!(lm.models, vec!["qwen2.5-coder-7b"]);
        assert_eq!(lm.model, "qwen2.5-coder-7b");
    }

    /// The panel shows a picker on exactly the engines that have one.
    #[test]
    fn the_view_says_which_engines_pick_a_model() {
        let v = view(&json!({}));
        let rows = v.as_array().unwrap();
        let picks = |id: &str| rows.iter().find(|r| r["id"] == id).unwrap()["picksModel"] == true;
        assert!(picks("claude"));
        assert!(picks("lmstudio"));
        assert!(!picks("shell"));
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
