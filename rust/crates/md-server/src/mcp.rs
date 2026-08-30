//! The default MCP bundle, and its consent model.
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

impl Entry {
    fn default_enabled(&self) -> bool {
        self.tier == "safe-readonly"
    }
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

/// The catalog as the consent UI shows it: what each server is, and whether it
/// is on for this tenant.
pub fn catalog_view(config: &Value) -> Value {
    json!(CATALOG
        .iter()
        .map(|e| {
            let consented = config.pointer(&format!("/mcpDefaults/{}/enabled", e.id))
                .and_then(|v| v.as_bool());
            json!({
                "id": e.id, "label": e.label, "description": e.description,
                "tier": e.tier,
                "enabled": enabled(e, consented),
                // Surfaced so the UI can explain WHY a server is off and what
                // turning it on means, rather than offering a bare switch.
                "requiresConsent": !e.default_enabled(),
            })
        })
        .collect::<Vec<Value>>())
}

fn enabled(e: &Entry, consented: Option<bool>) -> bool {
    match e.tier {
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
    for e in CATALOG {
        let consented = config.pointer(&format!("/mcpDefaults/{}/enabled", e.id))
            .and_then(|v| v.as_bool());
        if !enabled(e, consented) {
            continue;
        }
        let args: Vec<String> = e.args.iter()
            .map(|a| if *a == "<cwd>" { cwd.to_string() } else { (*a).to_string() })
            .collect();
        out.insert(format!("munder-{}", e.id), json!({ "command": e.command, "args": args }));
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
