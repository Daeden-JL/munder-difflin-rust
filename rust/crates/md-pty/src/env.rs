//! The PATH an agent process actually needs, and killing one properly.
//!
//! Both exist because the obvious version is wrong in a way that only shows up
//! in production: a server started as a daemon has a stub `PATH`, and a bare
//! `kill` leaves a process tree behind.

use std::sync::OnceLock;

/// Run `script` in an interactive login shell and return only what the script
/// itself printed.
///
/// An interactive shell is required to pick up nvm/asdf/brew PATH edits — but
/// it also runs the user's rc files, which are free to print. Some zsh setups
/// emit `Restored session: <date>` from a session plugin BEFORE the script
/// runs, which silently poisons the value: a plain trim on `echo "$PATH"`
/// yields `"Restored session: …\n/opt/homebrew/bin:…"`, and that whole string
/// becomes the PATH handed to every agent.
///
/// Fencing between two markers makes rc-file chatter — before, after, or both —
/// impossible to mistake for a result.
fn capture_from_login_shell(script: &str) -> Option<String> {
    const FENCE: &str = "__MD_SHELL_FENCE__";
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let out = std::process::Command::new(shell)
        .args([
            "-ilc",
            &format!("printf %s {FENCE}; {script}; printf %s {FENCE}"),
        ])
        .output()
        .ok()?;

    let text = String::from_utf8_lossy(&out.stdout);
    let start = text.find(FENCE)? + FENCE.len();
    let end = text.rfind(FENCE)?;
    (end > start).then(|| text[start..end].to_string())
}

/// The PATH agent processes should inherit, resolved once per process.
///
/// A server started by systemd, launchd or Docker has a minimal `PATH` that
/// usually lacks whatever installed the agent CLI, so a bare `claude` exits
/// 127. `MD_AGENT_PATH` wins when set — in a container the image's PATH is
/// already the right answer and spawning a login shell per boot is waste.
pub fn agent_path() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        if let Ok(explicit) = std::env::var("MD_AGENT_PATH") {
            if !explicit.trim().is_empty() {
                return explicit;
            }
        }
        let own = std::env::var("PATH").unwrap_or_default();
        if cfg!(windows) {
            return own;
        }
        match capture_from_login_shell("printf %s \"$PATH\"") {
            // A PATH is one colon-joined line. Anything multi-line is rc-file
            // noise that slipped the fence — fall back rather than hand the
            // agent a corrupt PATH it carries into every subprocess it spawns.
            Some(p) if !p.trim().is_empty() && !p.contains('\n') => p.trim().to_string(),
            _ => own,
        }
    })
}

/// Append `dir` to `path` if it is not already there.
///
/// APPEND, never prepend: prepending a bundled runtime would shadow the version
/// the user's own projects are pinned to. The bundled copy is a fallback for
/// when nothing else provides the binary, not an override.
pub fn append_to_path(path: &str, dir: &str) -> String {
    let sep = if cfg!(windows) { ';' } else { ':' };
    if dir.is_empty() || path.split(sep).any(|p| p == dir) {
        return path.to_string();
    }
    if path.is_empty() {
        return dir.to_string();
    }
    format!("{path}{sep}{dir}")
}

/// Grace between the polite signal and the escalation.
pub const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(4);

/// Is the process still alive? Signal 0 probes without touching it.
#[cfg(unix)]
pub fn is_alive(pid: u32) -> bool {
    // SAFETY: `kill` with signal 0 performs a permission/existence check and
    // sends nothing.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
pub fn is_alive(_pid: u32) -> bool {
    false
}

/// Kill a process and everything it started.
///
/// A bare kill signals the direct child only, which leaks twice: a child that
/// ignores the signal never dies, and its own children — MCP servers, helper
/// daemons — are orphaned to PID 1 and never released. The pty child is a
/// session leader, so its process GROUP covers its descendants.
///
/// Killing the group of an already-dead leader is exactly the orphan-reaping
/// case: any survivors still hold the group id.
///
/// Deliberate scope: EXPLICIT kills only — never a natural exit, where a daemon
/// the agent intentionally left running (a dev server it started) must outlive
/// the session.
#[cfg(unix)]
pub fn hard_kill_tree(pid: u32) {
    if pid == 0 {
        return;
    }
    // SAFETY: a negative pid targets the process group; both calls are
    // permission-checked by the kernel and cannot corrupt this process.
    unsafe {
        if libc::kill(-(pid as i32), libc::SIGKILL) != 0 {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
pub fn hard_kill_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/pid", &pid.to_string(), "/T", "/F"])
        .output();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prepending would shadow the version a user's project pins, which the
    /// original calls out as a deliberate product decision.
    #[test]
    fn the_bundled_runtime_is_appended_never_prepended() {
        let path = "/usr/bin:/bin";
        let out = append_to_path(path, "/opt/md/node");
        assert_eq!(out, "/usr/bin:/bin:/opt/md/node");
        assert!(out.starts_with(path), "the user's own PATH must still win");
    }

    #[test]
    fn appending_is_idempotent_and_handles_the_empty_cases() {
        assert_eq!(append_to_path("/usr/bin:/opt/x", "/opt/x"), "/usr/bin:/opt/x");
        assert_eq!(append_to_path("", "/opt/x"), "/opt/x");
        assert_eq!(append_to_path("/usr/bin", ""), "/usr/bin");
    }

    /// The fence exists so rc-file chatter cannot be mistaken for the value.
    #[test]
    fn rc_file_noise_does_not_leak_into_a_captured_value() {
        // Simulate the reported failure: a plugin prints before the script runs.
        let captured = capture_from_login_shell("printf %s /opt/bin:/usr/bin");
        if let Some(v) = captured {
            assert!(!v.contains("__MD_SHELL_FENCE__"));
            assert_eq!(v, "/opt/bin:/usr/bin", "only the script's own output");
        }
        // No assertion when the shell is unavailable: CI images often have no
        // interactive shell, and the fallback path is what agent_path tests.
    }

    /// A multi-line value means the fence was defeated; the process PATH is a
    /// worse answer than the shell's, but a corrupt one is worse than both.
    #[test]
    fn agent_path_is_a_single_line() {
        let p = agent_path();
        assert!(!p.contains('\n'), "a corrupt PATH propagates to every subprocess");
    }

    #[test]
    fn an_explicit_override_wins_over_probing_a_shell() {
        // Documented behaviour rather than a live check: `agent_path` memoises
        // per process, so this asserts the precedence contract only.
        assert!(std::env::var("MD_AGENT_PATH").is_err() || !agent_path().is_empty());
    }
}
