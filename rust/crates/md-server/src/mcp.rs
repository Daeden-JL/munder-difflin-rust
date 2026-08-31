//! The tools agents can reach, and the consent that arms them.
//!
//! Agents reach tools through MCP servers. The catalog exists so a user picks
//! from a vetted list rather than hand-writing launch specs, and the TIER is
//! the load-bearing part:
//!
//! * **safe-readonly** — no secrets, no writes outside the workspace. Shipped ON.
//! * **write** / **secret** — anything that writes beyond the workspace or needs
//!   a credential. Shipped OFF, and can only be turned on by an explicit
//!   `enabled: true`. A default can never arm one, so a partial or hand-edited
//!   config cannot silently give an agent a keyed server.
//!
//! The catalog is a starting point, not a whitelist: a tenant registers its own
//! servers under `mcpDefaults`, overriding any field of a built-in or adding one
//! the bundle has never heard of. A registration is a launch spec — a command
//! this harness will run — so a new one is treated as `write` unless it says
//! otherwise, and therefore lands OFF. The orchestrator can register one too
//! (see `handlers::harness_request`), and what it registers is always off and
//! always marked with who asked for it: an agent that could arm its own tool
//! could run any command it liked, which is not a tool system but a shell.

use serde_json::{json, Value};

/// One catalog entry, as the consent UI and the spawn merge both read it.
pub struct Entry {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    /// `safe-readonly` | `write` | `secret`
    pub tier: &'static str,
}

/// `<cwd>` is replaced with the agent's working directory at merge time, so the
/// filesystem and git servers stay scoped to the workspace rather than the disk.
pub const CATALOG: &[Entry] = &[
    Entry { id: "sequential-thinking", label: "Sequential Thinking",
        description: "A step-by-step reasoning scratchpad. No I/O, no secrets.",
        command: "npx", args: &["-y", "@modelcontextprotocol/server-sequential-thinking"],
        tier: "safe-readonly" },
    Entry { id: "time", label: "Time",
        description: "The current time and timezone conversion.",
        command: "npx", args: &["-y", "@modelcontextprotocol/server-time"],
        tier: "safe-readonly" },
    Entry { id: "fetch", label: "Fetch",
        description: "Read a URL. Network reads only.",
        command: "npx", args: &["-y", "@modelcontextprotocol/server-fetch"],
        tier: "safe-readonly" },
    Entry { id: "filesystem", label: "Filesystem",
        description: "Read files under the agent's workspace. Scoped to its cwd.",
        command: "npx", args: &["-y", "@modelcontextprotocol/server-filesystem", "<cwd>"],
        tier: "safe-readonly" },
    Entry { id: "git", label: "Git",
        description: "Inspect the workspace repository. Read-only.",
        command: "npx", args: &["-y", "@modelcontextprotocol/server-git", "--repository", "<cwd>"],
        tier: "safe-readonly" },
    Entry { id: "github", label: "GitHub",
        description: "Issues and pull requests. Needs a token, and can write.",
        command: "npx", args: &["-y", "@modelcontextprotocol/server-github"],
        tier: "secret" },
    Entry { id: "db", label: "Database",
        description: "Query a configured database. Needs a connection string.",
        command: "npx", args: &["-y", "@modelcontextprotocol/server-postgres"],
        tier: "secret" },
];

/// One server, built-in defaults merged with whatever this tenant changed.
#[derive(Debug, Clone)]
pub struct Server {
    pub id: String,
    pub label: String,
    pub description: String,
    pub command: String,
    pub args: Vec<String>,
    pub tier: String,
    pub enabled: bool,
    pub builtin: bool,
    /// The agent that asked for this one, if an agent did. Recorded so an
    /// operator arming it can see they are approving a request rather than
    /// their own earlier decision.
    pub proposed_by: Option<String>,
}

fn strings(v: Option<&Value>) -> Option<Vec<String>> {
    Some(v?.as_array()?.iter().filter_map(|a| a.as_str().map(String::from)).collect())
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str()).filter(|s| !s.trim().is_empty())
}

/// Every server this tenant has, built-ins first and then its own.
pub fn all(config: &Value) -> Vec<Server> {
    let over = config.get("mcpDefaults").and_then(|v| v.as_object());
    let patch_of = |id: &str| over.and_then(|o| o.get(id));

    let mut out: Vec<Server> = CATALOG
        .iter()
        .filter_map(|e| {
            let p = patch_of(e.id);
            if p.is_some_and(|v| v.get("hidden").and_then(|h| h.as_bool()).unwrap_or(false)) {
                return None;
            }
            let consented = p.and_then(|v| v.get("enabled")).and_then(|v| v.as_bool());
            let mut s = Server {
                id: e.id.into(),
                label: e.label.into(),
                description: e.description.into(),
                command: e.command.into(),
                args: e.args.iter().map(|a| (*a).to_string()).collect(),
                tier: e.tier.into(),
                enabled: enabled(e.tier, consented),
                builtin: true,
                proposed_by: None,
            };
            if let Some(v) = p {
                if let Some(x) = str_of(v, "label") { s.label = x.into(); }
                if let Some(x) = str_of(v, "description") { s.description = x.into(); }
                if let Some(x) = str_of(v, "command") { s.command = x.into(); }
                if let Some(x) = strings(v.get("args")) { s.args = x; }
            }
            Some(s)
        })
        .collect();

    if let Some(over) = over {
        for (id, v) in over {
            if CATALOG.iter().any(|e| e.id == id) {
                continue;
            }
            if v.get("hidden").and_then(|h| h.as_bool()).unwrap_or(false) {
                continue;
            }
            // A command is the one thing that cannot be defaulted: an entry
            // without one is a name that launches nothing.
            let Some(command) = str_of(v, "command") else { continue };
            // A registration the bundle has not vetted is treated as `write`
            // unless it says otherwise, so it lands off. The bundle's own
            // `safe-readonly` marks were reviewed; this one has not been.
            let tier = str_of(v, "tier").unwrap_or("write").to_string();
            let consented = v.get("enabled").and_then(|x| x.as_bool());
            out.push(Server {
                id: id.clone(),
                label: str_of(v, "label").unwrap_or(id).to_string(),
                description: str_of(v, "description")
                    .unwrap_or("Registered on this floor.")
                    .to_string(),
                command: command.to_string(),
                args: strings(v.get("args")).unwrap_or_default(),
                enabled: enabled(&tier, consented),
                tier,
                builtin: false,
                proposed_by: str_of(v, "proposedBy").map(String::from),
            });
        }
    }
    out
}

/// The catalog as the consent UI shows it: what each server is, and whether it
/// is on for this tenant.
pub fn catalog_view(config: &Value) -> Value {
    json!(all(config)
        .into_iter()
        .map(|s| json!({
            "id": s.id, "label": s.label, "description": s.description,
            "tier": s.tier, "enabled": s.enabled,
            "command": s.command, "args": s.args,
            "builtin": s.builtin,
            // Surfaced so the UI can explain WHY a server is off and what
            // turning it on means, rather than offering a bare switch.
            "requiresConsent": s.tier != "safe-readonly",
            "proposedBy": s.proposed_by,
        }))
        .collect::<Vec<Value>>())
}

fn enabled(tier: &str, consented: Option<bool>) -> bool {
    match tier {
        // A write or secret server needs an EXPLICIT yes. Absent consent is not
        // consent, and a default must never arm one.
        "safe-readonly" => consented.unwrap_or(true),
        _ => consented == Some(true),
    }
}

/// The `mcpServers` map written into an agent's session settings.
///
/// Ids are namespaced `munder-<id>` so a server of the same name in the user's
/// own `~/.claude` is never clobbered.
pub fn servers_for(cwd: &str, config: &Value) -> Value {
    let mut out = serde_json::Map::new();
    for s in all(config) {
        if !s.enabled {
            continue;
        }
        let args: Vec<String> = s
            .args
            .iter()
            .map(|a| if a == "<cwd>" { cwd.to_string() } else { a.clone() })
            .collect();
        out.insert(format!("munder-{}", s.id), json!({ "command": s.command, "args": args }));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule the whole tier system exists for: a keyed server can only be
    /// armed by an explicit yes, never by a default or a partial config.
    #[test]
    fn a_secret_server_needs_explicit_consent() {
        let none = json!({});
        let servers = servers_for("/w", &none);
        assert!(servers.get("munder-github").is_none(), "off without consent");
        assert!(servers.get("munder-filesystem").is_some(), "safe servers are on");

        // Absent is not consent, and neither is a half-written entry.
        let partial = json!({ "mcpDefaults": { "github": {} } });
        assert!(servers_for("/w", &partial).get("munder-github").is_none());

        let yes = json!({ "mcpDefaults": { "github": { "enabled": true } } });
        assert!(servers_for("/w", &yes).get("munder-github").is_some());
    }

    #[test]
    fn a_safe_server_can_be_turned_off() {
        let off = json!({ "mcpDefaults": { "filesystem": { "enabled": false } } });
        assert!(servers_for("/w", &off).get("munder-filesystem").is_none());
    }

    /// Scoped to the workspace, never the whole disk.
    #[test]
    fn the_workspace_servers_are_scoped_to_the_agent_cwd() {
        let servers = servers_for("/home/md/work/proj", &json!({}));
        let fs = &servers["munder-filesystem"]["args"];
        assert!(fs.as_array().unwrap().iter().any(|a| a == "/home/md/work/proj"));
        assert!(!fs.to_string().contains("<cwd>"), "the placeholder must be replaced");
    }

    /// Namespaced, so a user's own server of the same name is never clobbered.
    #[test]
    fn server_ids_are_namespaced() {
        let servers = servers_for("/w", &json!({}));
        assert!(servers.as_object().unwrap().keys().all(|k| k.starts_with("munder-")));
    }

    /// Registering a tool is registering a COMMAND this harness will run. The
    /// bundle's safe marks were reviewed; a new entry has not been, so it is
    /// treated as `write` and lands off until somebody says otherwise.
    #[test]
    fn a_registered_server_is_off_until_someone_says_yes() {
        let cfg = json!({ "mcpDefaults": { "mine": { "command": "my-server" } } });
        let row = catalog_view(&cfg);
        let mine = row.as_array().unwrap().iter().find(|r| r["id"] == "mine").unwrap();
        assert_eq!(mine["tier"], "write");
        assert_eq!(mine["enabled"], false);
        assert_eq!(mine["builtin"], false);
        assert!(servers_for("/w", &cfg).get("munder-mine").is_none());

        let on = json!({ "mcpDefaults": { "mine": { "command": "my-server", "enabled": true } } });
        assert_eq!(servers_for("/w", &on)["munder-mine"]["command"], "my-server");
    }

    /// Claiming a tool is safe is not the same as it being safe, but it IS the
    /// operator's call — a hand-written `safe-readonly` arms it, exactly as it
    /// does for a built-in.
    #[test]
    fn a_registration_can_declare_itself_safe_and_be_believed() {
        let cfg = json!({ "mcpDefaults": {
            "mine": { "command": "my-server", "tier": "safe-readonly" }
        }});
        assert!(servers_for("/w", &cfg).get("munder-mine").is_some());
    }

    /// What an agent asked for is never armed by the asking. Recorded, so the
    /// operator arming it knows they are approving a request.
    #[test]
    fn a_server_an_agent_proposed_is_off_and_says_who_asked() {
        let cfg = json!({ "mcpDefaults": { "scraper": {
            "command": "npx", "args": ["-y", "some-scraper"],
            "proposedBy": "michael", "enabled": false,
        }}});
        let v = catalog_view(&cfg);
        let row = v.as_array().unwrap().iter().find(|r| r["id"] == "scraper").unwrap();
        assert_eq!(row["proposedBy"], "michael");
        assert_eq!(row["enabled"], false);
        assert!(servers_for("/w", &cfg).get("munder-scraper").is_none());
    }

    #[test]
    fn a_registration_without_a_command_is_not_a_server() {
        let cfg = json!({ "mcpDefaults": { "empty": { "enabled": true } } });
        assert!(!catalog_view(&cfg).as_array().unwrap().iter().any(|r| r["id"] == "empty"));
        assert!(servers_for("/w", &cfg).get("munder-empty").is_none());
    }

    #[test]
    fn a_builtin_can_be_retargeted_without_losing_what_it_is() {
        let cfg = json!({ "mcpDefaults": { "fetch": { "command": "my-fetch" } } });
        assert_eq!(servers_for("/w", &cfg)["munder-fetch"]["command"], "my-fetch");
        let v = catalog_view(&cfg);
        let row = v.as_array().unwrap().iter().find(|r| r["id"] == "fetch").unwrap();
        assert_eq!(row["label"], "Fetch", "the description of what it is survives");
    }

    #[test]
    fn the_view_explains_which_servers_need_consent() {
        let view = catalog_view(&json!({}));
        let list = view.as_array().unwrap();
        assert_eq!(list.len(), CATALOG.len());
        let gh = list.iter().find(|e| e["id"] == "github").unwrap();
        assert_eq!(gh["enabled"], false);
        assert_eq!(gh["requiresConsent"], true);
        let fs = list.iter().find(|e| e["id"] == "filesystem").unwrap();
        assert_eq!(fs["enabled"], true);
        assert_eq!(fs["requiresConsent"], false);
    }
}
