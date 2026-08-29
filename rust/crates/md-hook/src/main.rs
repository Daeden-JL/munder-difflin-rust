//! `md-hook` — the shim an agent CLI runs on every lifecycle hook.
//!
//! Replaces Electron's `cth-hook.cjs`, which needed a Node runtime inside the
//! agent's environment. This is a static binary instead: the agent container no
//! longer has to contain a language runtime just to report that a tool ran.
//!
//! It is a byte relay, not a participant. Read the payload on stdin, write it to
//! the tenant's hook socket, copy the reply to stdout. It deliberately does not
//! parse the JSON: the payload schema belongs to the CLI and the reply schema to
//! the server, and a shim that understood either would need updating whenever
//! one of them changed.
//!
//! **It must never block the agent.** Every failure — no socket, no server, a
//! wedged server, a timeout — prints `{}` and exits 0, which the CLI reads as
//! "no hook opinion, carry on". A harness that is down has to be invisible to
//! the work, not a wedge in front of it.
//!
//! Usage: the socket path comes from `$MD_HOOK_SOCK`, or argv[1].

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// Generous next to a hook that normally answers in microseconds, and still far
/// below any CLI-side hook timeout — so when this does fire, the agent sees a
/// clean "no opinion" rather than a stall.
const TIMEOUT: Duration = Duration::from_secs(5);

/// A payload larger than this is not a hook; refuse to relay it rather than
/// hand the server an unbounded write.
const MAX_PAYLOAD: usize = 1024 * 1024;

fn main() {
    // Whatever happens, stdout carries valid JSON and the exit code is 0.
    let reply = relay().unwrap_or_else(|e| {
        eprintln!("md-hook: {e}");
        "{}".to_string()
    });
    let mut out = std::io::stdout();
    let _ = out.write_all(reply.as_bytes());
    let _ = out.flush();
}

fn relay() -> Result<String, String> {
    let sock = std::env::var("MD_HOOK_SOCK")
        .ok()
        .or_else(|| std::env::args().nth(1))
        .ok_or("no socket path (set MD_HOOK_SOCK or pass it as argv[1])")?;

    let mut payload = String::new();
    std::io::stdin()
        .take(MAX_PAYLOAD as u64)
        .read_to_string(&mut payload)
        .map_err(|e| format!("reading stdin: {e}"))?;

    let stream = UnixStream::connect(&sock).map_err(|e| format!("connecting to {sock}: {e}"))?;
    stream.set_read_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;

    let mut stream = stream;
    // The server reads one line, so the payload must be one line: a pretty-printed
    // payload would otherwise deliver only its first brace and hang the read.
    let line = payload.replace(['\n', '\r'], " ");
    stream
        .write_all(line.trim_end().as_bytes())
        .and_then(|()| stream.write_all(b"\n"))
        .map_err(|e| format!("writing payload: {e}"))?;
    // Half-close so a server reading to EOF is not left waiting on more input.
    let _ = stream.shutdown(std::net::Shutdown::Write);

    let mut reply = String::new();
    stream
        .take(MAX_PAYLOAD as u64)
        .read_to_string(&mut reply)
        .map_err(|e| format!("reading reply: {e}"))?;

    let reply = reply.trim().to_string();
    // An empty reply is a well-formed "no opinion"; passing it through as-is
    // would give the CLI an empty stdout to parse.
    Ok(if reply.is_empty() { "{}".into() } else { reply })
}
