//! The git plane: 14 read-mostly channels backing the repo panels.
//!
//! Every handler shells out to `git` rather than linking a git library. That
//! matches the Electron original byte for byte, which matters more than
//! elegance here — the renderer parses these shapes today, and a reimplementation
//! that is merely *equivalent* would still be a rewrite of the panels.
//!
//! Two guards, both ported deliberately:
//!   * `is_safe_rev` — a ref name reaches `git` as an argument, so a leading `-`
//!     would become a flag. Refuse those rather than trying to escape them.
//!   * `safe_join`   — a repo-relative path from the client must not escape the
//!     repo, independently of the tenant check the caller already did.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

/// Matches the Electron default. Long enough for a cold index on a big repo,
/// short enough that a wedged `git` cannot hold an RPC slot open.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);
/// Graph, diff and checkout walk more history, so they get the longer budget.
const SLOW_TIMEOUT: Duration = Duration::from_secs(15);
/// Keeps a diff payload small enough for the editor to stay responsive.
const MAX_DIFF_BYTES: u64 = 2 * 1024 * 1024;

pub enum GitOut {
    Ok(String),
    Err(String),
}

/// Run `git` in `cwd`. A non-zero exit is a value, not an error: most callers
/// turn it into `{error}` for the renderer, and a few (upstream lookup, HEAD
/// resolution) treat failure as a legitimate "not configured" answer.
async fn run(cwd: &Path, args: &[&str], timeout: Duration) -> GitOut {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        // A pager or a credential prompt would block forever on a server with no
        // terminal attached; both are disabled rather than relied upon to behave.
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0");

    match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => GitOut::Err(format!("git timed out after {}s", timeout.as_secs())),
        Ok(Err(e)) => GitOut::Err(e.to_string()),
        Ok(Ok(out)) if out.status.success() => {
            GitOut::Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            GitOut::Err(if stderr.is_empty() {
                format!("git exited {}", out.status.code().unwrap_or(-1))
            } else {
                stderr
            })
        }
    }
}

/// A revision the client supplied is about to become a command-line argument.
/// The leading-dash rejection is the load-bearing part: `--upload-pack=…` in a
/// ref position is a command execution, not a bad lookup.
pub fn is_safe_rev(rev: &str) -> bool {
    !rev.is_empty()
        && rev.len() <= 256
        && !rev.starts_with('-')
        && rev
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._/^~@{}-".contains(c))
}

/// Join a repo-relative path to the repo root, refusing anything that climbs
/// out. Lexical, like the tenant check: resolving symlinks here would be a
/// promise this layer cannot keep.
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let joined = root.join(rel);
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out.starts_with(root).then_some(out)
}

fn err(msg: impl Into<String>) -> Value {
    json!({ "error": msg.into() })
}

macro_rules! git {
    ($cwd:expr, $args:expr) => {
        match run($cwd, $args, DEFAULT_TIMEOUT).await {
            GitOut::Ok(s) => s,
            GitOut::Err(e) => return err(e),
        }
    };
    ($cwd:expr, $args:expr, $t:expr) => {
        match run($cwd, $args, $t).await {
            GitOut::Ok(s) => s,
            GitOut::Err(e) => return err(e),
        }
    };
}

// ── Record shapes ───────────────────────────────────────────────────────────
// Field names are camelCase to match what the renderer already destructures.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Commit {
    sha: String,
    short_sha: String,
    parents: Vec<String>,
    subject: String,
    author: String,
    time: i64,
    refs: Vec<String>,
}

#[derive(Serialize)]
struct StatusEntry {
    path: String,
    index: String,
    worktree: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileChange {
    path: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_path: Option<String>,
}

// ── Handlers ────────────────────────────────────────────────────────────────

pub async fn is_repo(cwd: &Path) -> Value {
    match run(cwd, &["rev-parse", "--is-inside-work-tree"], DEFAULT_TIMEOUT).await {
        GitOut::Ok(s) => json!(s.trim() == "true"),
        GitOut::Err(_) => json!(false),
    }
}

/// The MAIN working tree of the repo `cwd` belongs to. For a linked worktree
/// that is the original checkout, not the worktree directory — an agent's cwd
/// is `worktrees/<agent-id>`, whose name says nothing about the project.
pub async fn main_repo(cwd: &Path) -> Value {
    let out = match run(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        DEFAULT_TIMEOUT,
    )
    .await
    {
        GitOut::Ok(s) => s,
        GitOut::Err(_) => return Value::Null,
    };
    let git_dir = out.trim();
    if git_dir.is_empty() {
        return Value::Null;
    }
    // `<repo>/.git` → `<repo>`. A bare repo has no working tree to name, so its
    // own path is the best answer available.
    let stripped = git_dir
        .trim_end_matches(['/', '\\'])
        .strip_suffix(".git")
        .map(|s| s.trim_end_matches(['/', '\\']))
        .filter(|s| !s.is_empty())
        .unwrap_or(git_dir);
    json!(stripped)
}

pub async fn branch(cwd: &Path) -> Value {
    let out = git!(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let name = out.trim();
    if name == "HEAD" {
        json!({ "current": Value::Null, "detached": true })
    } else {
        json!({ "current": name, "detached": false })
    }
}

pub async fn status(cwd: &Path) -> Value {
    let out = git!(
        cwd,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"]
    );
    let mut entries = Vec::new();
    let mut untracked = Vec::new();
    for token in out.split('\0').filter(|t| !t.is_empty()) {
        if token.len() < 3 {
            continue;
        }
        let mut chars = token.chars();
        let (index, worktree) = (chars.next().unwrap(), chars.next().unwrap());
        // Byte index 3 is safe: the two status chars and the space are ASCII.
        let path = token[3..].to_string();
        if index == '?' && worktree == '?' {
            untracked.push(path);
        } else {
            entries.push(StatusEntry {
                path,
                index: index.to_string(),
                worktree: worktree.to_string(),
            });
        }
    }
    let staged: Vec<_> = entries.iter().filter(|e| e.index != " " && e.index != "?").collect();
    let unstaged: Vec<_> = entries.iter().filter(|e| e.worktree != " " && e.worktree != "?").collect();
    json!({ "staged": staged, "unstaged": unstaged, "untracked": untracked })
}

/// `%x1e`/`%x1f` separate records and fields: a commit subject can contain any
/// byte a line-based format would use as a delimiter.
const REC: char = '\x1e';
const FLD: char = '\x1f';

fn commit_format() -> String {
    format!("format:%H{FLD}%P{FLD}%s{FLD}%an{FLD}%at{FLD}%D{REC}")
}

fn parse_commits(stdout: &str) -> Vec<Commit> {
    let mut out = Vec::new();
    for rec in stdout.split(REC) {
        if rec.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = rec.split(FLD).collect();
        let Some(sha) = f.first().map(|s| s.trim()) else { continue };
        if sha.is_empty() {
            continue;
        }
        out.push(Commit {
            sha: sha.to_string(),
            short_sha: sha.chars().take(7).collect(),
            parents: f.get(1).map(|p| p.split_whitespace().map(String::from).collect()).unwrap_or_default(),
            subject: f.get(2).unwrap_or(&"").to_string(),
            author: f.get(3).unwrap_or(&"").to_string(),
            time: f.get(4).and_then(|t| t.trim().parse().ok()).unwrap_or(0),
            refs: f
                .get(5)
                .map(|r| r.split(", ").map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
                .unwrap_or_default(),
        });
    }
    out
}

pub async fn log(cwd: &Path, n: u32) -> Value {
    let pretty = format!("--pretty={}", commit_format());
    let max = format!("--max-count={n}");
    let out = git!(cwd, &["log", "--all", &max, &pretty]);
    json!(parse_commits(&out))
}

/// Same records as `log`, but topologically ordered — the graph layout depends
/// on that — and pageable.
pub async fn log_graph(cwd: &Path, n: u32, skip: u32) -> Value {
    let pretty = format!("--pretty={}", commit_format());
    let max = format!("--max-count={n}");
    let skip_arg = format!("--skip={skip}");
    let mut args = vec!["log", "--all", "--topo-order", &max];
    if skip > 0 {
        args.push(&skip_arg);
    }
    args.push(&pretty);
    let out = git!(cwd, &args, SLOW_TIMEOUT);
    json!(parse_commits(&out))
}

pub async fn branches(cwd: &Path) -> Value {
    let fmt = format!("--format=%(HEAD){FLD}%(refname:short)");
    let out = git!(cwd, &["branch", "-a", &fmt]);
    let (mut current, mut local, mut remote) = (Value::Null, Vec::new(), Vec::new());
    for line in out.lines().filter(|l| !l.is_empty()) {
        let Some((head, name)) = line.split_once(FLD) else { continue };
        if name.is_empty() {
            continue;
        }
        if head.trim() == "*" {
            current = json!(name);
        }
        match name.strip_prefix("remotes/") {
            Some(r) => remote.push(r.to_string()),
            None => local.push(name.to_string()),
        }
    }
    json!({ "local": local, "remote": remote, "current": current })
}

/// No upstream is a normal state, not an error — an unpublished branch reports
/// zeroes rather than failing the panel.
pub async fn ahead_behind(cwd: &Path) -> Value {
    let up = match run(
        cwd,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        DEFAULT_TIMEOUT,
    )
    .await
    {
        GitOut::Ok(s) => s.trim().to_string(),
        GitOut::Err(_) => return json!({ "ahead": 0, "behind": 0, "upstream": Value::Null }),
    };
    let range = format!("HEAD...{up}");
    let out = git!(cwd, &["rev-list", "--left-right", "--count", &range]);
    let mut it = out.trim().split('\t').map(|n| n.parse::<i64>().unwrap_or(0));
    json!({
        "ahead": it.next().unwrap_or(0),
        "behind": it.next().unwrap_or(0),
        "upstream": up,
    })
}

fn parse_name_status_z(stdout: &str) -> Vec<FileChange> {
    let tokens: Vec<&str> = stdout.split('\0').filter(|t| !t.is_empty()).collect();
    let mut files = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let status = tokens[i];
        // Skip stray commit ids that `diff-tree` can emit ahead of the records.
        let Some(kind) = status.chars().next().filter(|c| c.is_ascii_uppercase()) else {
            i += 1;
            continue;
        };
        // Renames and copies carry both sides: STATUS, old, new.
        if kind == 'R' || kind == 'C' {
            if let (Some(old), Some(path)) = (tokens.get(i + 1), tokens.get(i + 2)) {
                files.push(FileChange {
                    path: path.to_string(),
                    status: kind.to_string(),
                    old_path: Some(old.to_string()),
                });
            }
            i += 3;
        } else {
            if let Some(path) = tokens.get(i + 1) {
                files.push(FileChange {
                    path: path.to_string(),
                    status: kind.to_string(),
                    old_path: None,
                });
            }
            i += 2;
        }
    }
    files
}

pub async fn commit_files(cwd: &Path, sha: &str) -> Value {
    if !is_safe_rev(sha) {
        return err("invalid revision");
    }
    let out = git!(
        cwd,
        &["diff-tree", "--no-commit-id", "-r", "--root", "-z", "-M", "--name-status", sha, "--"]
    );
    json!(parse_name_status_z(&out))
}

/// Working-tree-vs-HEAD for one file, as the two sides a diff editor wants.
/// An empty `head` with `headExists: false` is a new file; the mirror case is a
/// deletion. Binary on either side blanks both — there is nothing to show.
pub async fn diff(cwd: &Path, rel_path: &str) -> Value {
    let Some(abs) = safe_join(cwd, rel_path) else {
        return json!({ "ok": false, "error": "path escapes repository root" });
    };

    let spec = format!("HEAD:{rel_path}");
    let (head, head_exists) = match run(cwd, &["show", &spec], DEFAULT_TIMEOUT).await {
        GitOut::Ok(s) => (s, true),
        GitOut::Err(_) => (String::new(), false),
    };

    let mut working = String::new();
    let mut working_exists = false;
    let mut working_binary = false;
    if let Ok(meta) = tokio::fs::metadata(&abs).await {
        if meta.len() > MAX_DIFF_BYTES {
            return json!({
                "ok": false,
                "error": format!("file too large to diff ({:.1} MB)", meta.len() as f64 / 1048576.0),
            });
        }
        if let Ok(buf) = tokio::fs::read(&abs).await {
            working_exists = true;
            if buf.contains(&0) {
                working_binary = true;
            } else {
                working = String::from_utf8_lossy(&buf).into_owned();
            }
        }
    }

    let is_binary = working_binary || head.contains('\0');
    json!({
        "ok": true,
        "path": abs,
        "relPath": rel_path,
        "head": if is_binary { "" } else { &head },
        "working": if is_binary { "" } else { &working },
        "headExists": head_exists,
        "workingExists": working_exists,
        "isBinary": is_binary,
    })
}

/// One side of a rev-pinned diff. A path absent at that rev is not an error —
/// it is the parent side of an added file — so it answers `exists: false`.
pub async fn show_file(cwd: &Path, rev: &str, rel_path: &str) -> Value {
    if !is_safe_rev(rev) {
        return json!({ "ok": false, "error": "invalid revision" });
    }
    if safe_join(cwd, rel_path).is_none() {
        return json!({ "ok": false, "error": "path escapes repository root" });
    }
    let spec = format!("{rev}:{rel_path}");
    let absent = json!({ "ok": true, "exists": false, "isBinary": false, "content": "" });

    match run(cwd, &["cat-file", "-s", &spec], DEFAULT_TIMEOUT).await {
        GitOut::Err(_) => return absent,
        GitOut::Ok(s) => {
            if s.trim().parse::<u64>().unwrap_or(0) > MAX_DIFF_BYTES {
                return json!({ "ok": false, "error": "file too large to diff (>2 MB)" });
            }
        }
    }
    match run(cwd, &["show", &spec], DEFAULT_TIMEOUT).await {
        GitOut::Err(_) => absent,
        GitOut::Ok(content) if content.contains('\0') => {
            json!({ "ok": true, "exists": true, "isBinary": true, "content": "" })
        }
        GitOut::Ok(content) => json!({ "ok": true, "exists": true, "isBinary": false, "content": content }),
    }
}

/// `three` (the default, PR-style) is what `head` adds since the merge base;
/// `two` is the literal state difference. They differ whenever base has moved.
pub async fn compare_refs(cwd: &Path, base: &str, head: &str, mode: &str) -> Value {
    if !is_safe_rev(base) || !is_safe_rev(head) {
        return err("invalid revision");
    }
    let sym = format!("{base}...{head}");
    let counts = git!(cwd, &["rev-list", "--left-right", "--count", &sym]);
    let mut it = counts.trim().split('\t').map(|x| x.parse::<i64>().unwrap_or(0));
    // Left of `...` is base, so the left count is how far head is BEHIND.
    let (behind, ahead) = (it.next().unwrap_or(0), it.next().unwrap_or(0));

    let merge_base = match run(cwd, &["merge-base", base, head], DEFAULT_TIMEOUT).await {
        GitOut::Ok(s) => json!(s.trim()),
        GitOut::Err(_) => Value::Null,
    };

    let args: Vec<&str> = if mode == "three" {
        vec!["diff", "-z", "-M", "--name-status", &sym, "--"]
    } else {
        vec!["diff", "-z", "-M", "--name-status", base, head, "--"]
    };
    let out = git!(cwd, &args, SLOW_TIMEOUT);
    json!({ "ahead": ahead, "behind": behind, "mergeBase": merge_base, "files": parse_name_status_z(&out) })
}

pub async fn worktrees(cwd: &Path) -> Value {
    let out = git!(cwd, &["worktree", "list", "--porcelain"]);
    let mut list = Vec::new();
    let mut cur: Option<(String, String, Value)> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some((path, head, branch)) = cur.take() {
                list.push(json!({ "path": path, "head": head, "branch": branch }));
            }
            cur = Some((p.to_string(), String::new(), Value::Null));
        } else if let Some(h) = line.strip_prefix("HEAD ") {
            if let Some(c) = cur.as_mut() {
                c.1 = h.to_string();
            }
        } else if let Some(b) = line.strip_prefix("branch ") {
            if let Some(c) = cur.as_mut() {
                c.2 = json!(b.strip_prefix("refs/heads/").unwrap_or(b));
            }
        }
    }
    if let Some((path, head, branch)) = cur {
        list.push(json!({ "path": path, "head": head, "branch": branch }));
    }
    json!(list)
}

/// The only mutating channel in this module, so it fails safe: any doubt about
/// the tree being clean refuses rather than risking uncommitted work. The
/// "branch is checked out elsewhere" case is translated into the holding
/// worktree's path, which is the thing the user actually needs to know.
pub async fn checkout(cwd: &Path, rev: &str, detach: bool) -> Value {
    if !is_safe_rev(rev) {
        return json!({ "ok": false, "error": "invalid revision" });
    }
    let st = status(cwd).await;
    if let Some(e) = st.get("error").and_then(|v| v.as_str()) {
        return json!({ "ok": false, "error": format!("could not verify a clean tree: {e}") });
    }
    let count = |k: &str| st.get(k).and_then(|v| v.as_array()).map_or(0, |a| a.len());
    let dirty = count("staged") + count("unstaged");
    if dirty > 0 {
        return json!({
            "ok": false,
            "error": format!(
                "working tree has {dirty} uncommitted change{} — commit or stash first",
                if dirty == 1 { "" } else { "s" }
            ),
        });
    }

    let args: Vec<&str> = if detach {
        vec!["switch", "--detach", rev]
    } else {
        vec!["switch", rev]
    };
    match run(cwd, &args, SLOW_TIMEOUT).await {
        GitOut::Ok(_) => json!({ "ok": true, "detached": detach }),
        GitOut::Err(e) => {
            let taken = e.contains("already checked out") || e.contains("already used by worktree");
            if taken {
                let want = rev.strip_prefix("refs/heads/").unwrap_or(rev);
                if let Some(holder) = worktrees(cwd)
                    .await
                    .as_array()
                    .and_then(|ws| {
                        ws.iter()
                            .find(|w| w.get("branch").and_then(|b| b.as_str()) == Some(want))
                            .and_then(|w| w.get("path").and_then(|p| p.as_str()).map(String::from))
                    })
                {
                    return json!({
                        "ok": false,
                        "error": format!("'{rev}' is checked out in another worktree: {holder}"),
                    });
                }
            }
            json!({ "ok": false, "error": e })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The leading-dash case is the one that matters: a ref that reaches `git`
    /// as `--upload-pack=…` is command execution, not a failed lookup.
    #[test]
    fn rejects_revisions_that_could_become_flags() {
        assert!(is_safe_rev("main"));
        assert!(is_safe_rev("v1.2.3"));
        assert!(is_safe_rev("HEAD~2^{commit}"));
        assert!(is_safe_rev("feature/thing-2"));
        assert!(!is_safe_rev("--upload-pack=touch /tmp/pwn"));
        assert!(!is_safe_rev("-x"));
        assert!(!is_safe_rev(""));
        assert!(!is_safe_rev("a b"));
        assert!(!is_safe_rev("head;rm -rf /"));
        assert!(!is_safe_rev(&"a".repeat(257)));
    }

    #[test]
    fn relative_paths_cannot_climb_out_of_the_repo() {
        let root = Path::new("/repo");
        assert_eq!(safe_join(root, "src/main.rs"), Some(PathBuf::from("/repo/src/main.rs")));
        assert_eq!(safe_join(root, "a/../b"), Some(PathBuf::from("/repo/b")));
        assert_eq!(safe_join(root, "../etc/passwd"), None);
        assert_eq!(safe_join(root, "src/../../etc/passwd"), None);
    }

    /// A subject containing the characters a line-based format would delimit on
    /// must survive intact.
    #[test]
    fn commit_records_survive_hostile_subjects() {
        let raw = format!(
            "abc123def456{FLD}p1 p2{FLD}fix: tabs\tand, commas{FLD}Dae{FLD}1700000000{FLD}HEAD -> main, origin/main{REC}"
        );
        let c = parse_commits(&raw);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].short_sha, "abc123d");
        assert_eq!(c[0].parents, vec!["p1", "p2"]);
        assert_eq!(c[0].subject, "fix: tabs\tand, commas");
        assert_eq!(c[0].time, 1_700_000_000);
        assert_eq!(c[0].refs, vec!["HEAD -> main", "origin/main"]);
    }

    /// Renames consume three tokens and everything else two; getting this wrong
    /// silently shifts every subsequent file by one.
    #[test]
    fn name_status_pairs_renames_with_both_sides() {
        let out = parse_name_status_z("M\0a.rs\0R100\0old.rs\0new.rs\0D\0gone.rs\0");
        assert_eq!(out.len(), 3);
        assert_eq!((out[0].status.as_str(), out[0].path.as_str()), ("M", "a.rs"));
        assert_eq!(out[1].old_path.as_deref(), Some("old.rs"));
        assert_eq!(out[1].path, "new.rs");
        assert_eq!((out[2].status.as_str(), out[2].path.as_str()), ("D", "gone.rs"));
    }
}
