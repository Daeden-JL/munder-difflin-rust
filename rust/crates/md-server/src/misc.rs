//! The long tail: small, independent channels that do not earn a module each.
//!
//! Grouped rather than scattered so the shape is visible — most of these are a
//! config read, a directory scan, or a shell-out to a CLI the user already has.

use std::path::Path;

use serde_json::{json, Value};

/// Run a CLI and parse its stdout as JSON.
///
/// Every failure — the binary is missing, it exited non-zero, the output is not
/// JSON — becomes `{ok: false, error}` rather than an exception, so a caller
/// never has to distinguish "not installed" from "not a repo". A timeout is
/// mandatory: `gh` can sit waiting on an auth prompt forever.
pub async fn cli_json(program: &str, args: &[&str], cwd: &Path) -> Result<Value, String> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::process::Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            // A prompt would hang a request until the timeout; failing fast is
            // a better answer than a stalled panel.
            .env("GH_PROMPT_DISABLED", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output(),
    )
    .await
    .map_err(|_| format!("{program} timed out"))?
    .map_err(|e| format!("{program} is not available: {e}"))?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() { format!("{program} failed") } else { err });
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("{program} returned unparseable JSON: {e}"))
}

/// Issues in the repo at `cwd`, flattened the way the panel renders them.
pub async fn github_issues(cwd: &Path) -> Value {
    let raw = match cli_json(
        "gh",
        &["issue", "list", "--limit", "30", "--json",
          "number,title,body,url,state,labels,assignees"],
        cwd,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => return json!({ "ok": false, "error": e }),
    };

    let issues: Vec<Value> = raw
        .as_array()
        .map(|list| {
            list.iter()
                .map(|i| {
                    json!({
                        "number": i["number"], "title": i["title"], "body": i["body"],
                        "url": i["url"],
                        // Flattened to names: the panel shows labels as chips and
                        // has no use for the rest of the object.
                        "labels": names(&i["labels"], "name"),
                        "assignees": names(&i["assignees"], "login"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({ "ok": true, "issues": issues })
}

pub async fn github_ci_runs(cwd: &Path) -> Value {
    match cli_json(
        "gh",
        &["run", "list", "--limit", "20", "--json",
          "databaseId,displayTitle,status,conclusion,headBranch,createdAt,url"],
        cwd,
    )
    .await
    {
        Ok(runs) => json!({ "ok": true, "runs": runs }),
        Err(e) => json!({ "ok": false, "error": e }),
    }
}

fn names(v: &Value, key: &str) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x[key].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Which agent CLIs this server can actually launch.
///
/// Probes the PATH agents will be spawned with, not the server's own — those
/// differ, and reporting the wrong one produces a UI that says a CLI is present
/// while every spawn exits 127.
pub fn tools_status() -> Value {
    const TOOLS: [&str; 7] = ["claude", "codex", "gemini", "grok", "qwen", "opencode", "gh"];
    let path = md_pty::env::agent_path();
    json!(TOOLS
        .iter()
        .map(|t| {
            let found = which(t, path);
            json!({ "id": t, "installed": found.is_some(), "path": found })
        })
        .collect::<Vec<Value>>())
}

/// Resolve a binary against a PATH, without spawning anything.
fn which(bin: &str, path: &str) -> Option<String> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    path.split(sep)
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(bin))
        .find(|p| is_executable(p))
        .map(|p| p.display().to_string())
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Skills available to an agent working in `cwd`.
///
/// A skill is a directory holding `SKILL.md`. Both scopes are scanned because a
/// project-scoped skill should appear where it applies, and a home-scoped one
/// everywhere.
pub fn local_skills(home: &Path, cwd: &Path) -> Value {
    let mut out = Vec::new();
    for (root, scope) in [(home.join(".claude/skills"), "user"), (cwd.join(".claude/skills"), "project")] {
        let Ok(dir) = std::fs::read_dir(&root) else { continue };
        for e in dir.filter_map(Result::ok) {
            let manifest = e.path().join("SKILL.md");
            if !manifest.is_file() {
                continue;
            }
            let text = std::fs::read_to_string(&manifest).unwrap_or_default();
            out.push(json!({
                "name": e.file_name().to_string_lossy(),
                "path": e.path(),
                "scope": scope,
                "description": front_matter(&text, "description"),
            }));
        }
    }
    json!(out)
}

/// Pull one field out of a `---` front-matter block.
///
/// Deliberately not a YAML parser: the two fields anyone reads are `name` and
/// `description`, and a full parser would be a dependency and an attack surface
/// for a file an agent may have written.
fn front_matter(text: &str, key: &str) -> Value {
    let Some(rest) = text.strip_prefix("---") else { return Value::Null };
    let Some(end) = rest.find("\n---") else { return Value::Null };
    for line in rest[..end].lines() {
        if let Some(v) = line.strip_prefix(&format!("{key}:")) {
            return json!(v.trim().trim_matches('"').trim_matches('\''));
        }
    }
    Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "md-misc-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn front_matter_reads_a_field_and_tolerates_a_file_without_one() {
        let doc = "---\nname: reviewer\ndescription: \"Reviews a diff\"\n---\n\n# body";
        assert_eq!(front_matter(doc, "description"), json!("Reviews a diff"));
        assert_eq!(front_matter(doc, "name"), json!("reviewer"));
        assert_eq!(front_matter(doc, "missing"), Value::Null);
        assert_eq!(front_matter("# no front matter", "name"), Value::Null);
        // An unterminated block must not be read as if it closed.
        assert_eq!(front_matter("---\nname: x\n", "name"), Value::Null);
    }

    /// A project skill should appear where it applies; a home skill everywhere.
    #[test]
    fn skills_are_found_in_both_scopes_and_need_a_manifest() {
        let (home, cwd) = (tmp(), tmp());
        for (root, name) in [(home.join(".claude/skills"), "global-one"), (cwd.join(".claude/skills"), "project-one")] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(
                root.join(name).join("SKILL.md"),
                format!("---\ndescription: does {name}\n---\n"),
            )
            .unwrap();
        }
        // A directory with no manifest is not a skill.
        std::fs::create_dir_all(cwd.join(".claude/skills/not-a-skill")).unwrap();

        let out = local_skills(&home, &cwd);
        let list = out.as_array().unwrap();
        assert_eq!(list.len(), 2);
        let scopes: Vec<&str> = list.iter().map(|s| s["scope"].as_str().unwrap()).collect();
        assert!(scopes.contains(&"user") && scopes.contains(&"project"));
        assert!(list.iter().any(|s| s["description"] == "does project-one"));
    }

    #[test]
    fn a_missing_skills_directory_is_empty_not_an_error() {
        assert_eq!(local_skills(Path::new("/nope"), Path::new("/also-nope")), json!([]));
    }

    /// Reporting the server's own PATH would produce a UI claiming a CLI is
    /// installed while every spawn exits 127.
    #[test]
    fn tool_status_probes_the_agent_path() {
        let dir = tmp();
        let bin = dir.join("claude");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(which("claude", &dir.display().to_string()), Some(bin.display().to_string()));
        assert_eq!(which("claude", "/nonexistent"), None);

        // A non-executable file of the right name is not a tool.
        let dir2 = tmp();
        std::fs::write(dir2.join("codex"), "text").unwrap();
        #[cfg(unix)]
        assert_eq!(which("codex", &dir2.display().to_string()), None);

        let status = tools_status();
        assert_eq!(status.as_array().unwrap().len(), 7);
        assert!(status.as_array().unwrap().iter().all(|t| t["id"].is_string()));
    }

    #[test]
    fn github_fields_are_flattened_to_names() {
        let raw = json!([{ "labels": [{ "name": "bug" }, { "name": "p1" }],
                           "assignees": [{ "login": "dae" }] }]);
        assert_eq!(names(&raw[0]["labels"], "name"), ["bug", "p1"]);
        assert_eq!(names(&raw[0]["assignees"], "login"), ["dae"]);
        assert!(names(&Value::Null, "name").is_empty());
    }
}
