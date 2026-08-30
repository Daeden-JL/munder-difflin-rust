//! The terminal plane: spawn agents on a pty and stream their bytes.
//!
//! Ports `src/main/pty.ts`. Two invariants from the Electron implementation are
//! load-bearing and preserved here:
//!
//! * **Streams are owned, never broadcast.** In Electron each `PtySession` kept
//!   the `WebContents` that spawned it so `pty:data:<id>` reached only that
//!   window. Here the owner is a tenant, and the consequence of getting it wrong
//!   is a cross-tenant leak rather than a cross-window one.
//! * **`last_output_at` / `has_output` gate automation.** The hive types into
//!   live PTYs, and typing mid-stream corrupts the TUI. Callers wait for
//!   `has_output` before the first write and check quiescence before nudging.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use md_tenant::{Sandbox, SpawnRequest, TenantId, TenantPaths};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("no pty session with id {0}")]
    NoSuchSession(String),
    #[error("session {0} belongs to another tenant")]
    WrongTenant(String),
    #[error("a session with id {0} already exists")]
    DuplicateId(String),
    #[error(transparent)]
    Sandbox(#[from] md_tenant::sandbox::SandboxError),
    #[error("{0}")]
    Spawn(String),
}

/// One frame from a pty. Exit is a terminal event: the receiver ends after it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PtyEvent {
    /// Raw bytes as UTF-8. Lossy by necessity — a pty can split a multi-byte
    /// sequence across reads, and the terminal emulator downstream re-assembles.
    Data { data: String },
    Exit { exit_code: i32, signal: Option<i32> },
    /// The agent was restarted into this same session after a first-run CLI
    /// install; the client re-arms the terminal in place instead of showing a
    /// dead one.
    Relaunch,
}

/// What `pty:list` reports.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PtyInfo {
    /// Current size, so a caller can nudge a redraw without guessing it.
    pub cols: u16,
    pub rows: u16,
    pub id: String,
    pub cwd: String,
    pub command: String,
    pub pid: u32,
    pub last_output_at: u64,
    pub has_output: bool,
}

struct Session {
    id: String,
    tenant: TenantId,
    cwd: String,
    command: String,
    pid: u32,
    /// Bounded: a runaway agent must not grow the buffer without limit. Slow
    /// subscribers lag and are told so by `RecvError::Lagged`, which the client
    /// handles by requesting a redraw — the same recovery the Electron version
    /// used for a reattaching terminal.
    tx: broadcast::Sender<PtyEvent>,
    input: mpsc::UnboundedSender<Vec<u8>>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// Kill handle only. The child itself is owned by the waiter thread, which is
    /// the single place it is reaped — two owners calling `wait()` would race.
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    last_output_at: Arc<Mutex<u64>>,
    has_output: Arc<Mutex<bool>>,
    /// Last size applied. Held so a redraw can round-trip the size without the
    /// caller having to know it — guessing would resize the agent's terminal.
    cols: Mutex<u16>,
    rows: Mutex<u16>,
}

pub mod env;

pub struct PtyManager {
    /// Shared with each session's waiter thread so a session can remove itself
    /// on exit; otherwise dead sessions accumulate in `pty:list` forever.
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    sandbox: Arc<dyn Sandbox>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl PtyManager {
    pub fn new(sandbox: Arc<dyn Sandbox>) -> Self {
        Self { sessions: Arc::new(Mutex::new(HashMap::new())), sandbox }
    }

    /// Spawn an agent. The command is produced by the sandbox, never by this
    /// function, so there is no path here that reaches an unconfined process.
    pub fn spawn(
        &self,
        id: &str,
        req: &SpawnRequest,
        paths: &TenantPaths,
    ) -> Result<broadcast::Receiver<PtyEvent>, PtyError> {
        {
            let sessions = self.sessions.lock().unwrap();
            if sessions.contains_key(id) {
                return Err(PtyError::DuplicateId(id.to_string()));
            }
        }

        let resolved = self.sandbox.resolve(req, paths)?;

        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize { rows: req.rows, cols: req.cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| PtyError::Spawn(format!("openpty: {e}")))?;

        let mut cmd = CommandBuilder::new(&resolved.program);
        cmd.args(&resolved.args);
        cmd.cwd(&resolved.cwd);
        // PATH first, so an explicit one in the request can still override it.
        // A server started as a daemon inherits a minimal PATH that usually
        // lacks whatever installed the agent CLI, and a bare `claude` then
        // exits 127 — which reads as "the agent crashed", not "PATH is wrong".
        cmd.env("PATH", env::agent_path());
        for (k, v) in &resolved.env {
            cmd.env(k, v);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| PtyError::Spawn(format!("spawn {}: {e}", resolved.program)))?;
        let pid = child.process_id().unwrap_or(0);
        let killer = child.clone_killer();
        // The slave fd must close in the parent or the reader never sees EOF when
        // the child exits, and the terminal hangs open forever.
        drop(pair.slave);

        let (tx, rx) = broadcast::channel(1024);
        let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        let last_output_at = Arc::new(Mutex::new(now_ms()));
        let has_output = Arc::new(Mutex::new(false));

        // Reader: blocking reads on the pty master, so it owns a dedicated thread
        // rather than occupying a tokio worker.
        {
            let tx = tx.clone();
            let last = Arc::clone(&last_output_at);
            let seen = Arc::clone(&has_output);
            let mut reader = pair
                .master
                .try_clone_reader()
                .map_err(|e| PtyError::Spawn(format!("clone reader: {e}")))?;
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            *last.lock().unwrap() = now_ms();
                            *seen.lock().unwrap() = true;
                            let data = String::from_utf8_lossy(&buf[..n]).into_owned();
                            // Send failure means every subscriber is gone; the
                            // session is still alive, so keep draining rather
                            // than killing the child.
                            let _ = tx.send(PtyEvent::Data { data });
                        }
                    }
                }
            });
        }

        // Writer: keystrokes and resizes, both of which must not block the async
        // runtime either.
        {
            let mut writer = pair
                .master
                .take_writer()
                .map_err(|e| PtyError::Spawn(format!("take writer: {e}")))?;
            std::thread::spawn(move || {
                while let Some(bytes) = input_rx.blocking_recv() {
                    if writer.write_all(&bytes).is_err() { break; }
                    let _ = writer.flush();
                }
            });
        }

        {
            let tx = tx.clone();
            let sessions = Arc::clone(&self.sessions);
            let sid = id.to_string();
            std::thread::spawn(move || {
                let code = child.wait().map(|st| st.exit_code() as i32).unwrap_or(-1);
                // Deregister BEFORE announcing, so a client reacting to the exit
                // by listing sessions never sees the dead one.
                sessions.lock().unwrap().remove(&sid);
                let _ = tx.send(PtyEvent::Exit { exit_code: code, signal: None });
            });
        }

        let session = Session {
            id: id.to_string(),
            tenant: req.tenant.clone(),
            cwd: resolved.cwd.display().to_string(),
            command: req.program.clone(),
            pid,
            tx,
            input: input_tx,
            master: pair.master,
            killer,
            last_output_at,
            has_output,
            cols: Mutex::new(req.cols),
            rows: Mutex::new(req.rows),
        };
        self.sessions.lock().unwrap().insert(id.to_string(), session);
        Ok(rx)
    }

    /// Subscribe an additional consumer (a second browser tab on the same
    /// session). Tenant-checked: this is the call a cross-tenant attacker would
    /// aim at, since it needs only a session id.
    pub fn subscribe(
        &self,
        id: &str,
        tenant: &TenantId,
    ) -> Result<broadcast::Receiver<PtyEvent>, PtyError> {
        let sessions = self.sessions.lock().unwrap();
        let s = sessions.get(id).ok_or_else(|| PtyError::NoSuchSession(id.into()))?;
        if &s.tenant != tenant {
            return Err(PtyError::WrongTenant(id.into()));
        }
        Ok(s.tx.subscribe())
    }

    pub fn write(&self, id: &str, tenant: &TenantId, data: &str) -> Result<(), PtyError> {
        let sessions = self.sessions.lock().unwrap();
        let s = sessions.get(id).ok_or_else(|| PtyError::NoSuchSession(id.into()))?;
        if &s.tenant != tenant {
            return Err(PtyError::WrongTenant(id.into()));
        }
        s.input.send(data.as_bytes().to_vec())
            .map_err(|_| PtyError::NoSuchSession(id.into()))
    }

    pub fn resize(&self, id: &str, tenant: &TenantId, cols: u16, rows: u16)
        -> Result<(), PtyError>
    {
        let sessions = self.sessions.lock().unwrap();
        let s = sessions.get(id).ok_or_else(|| PtyError::NoSuchSession(id.into()))?;
        if &s.tenant != tenant {
            return Err(PtyError::WrongTenant(id.into()));
        }
        s.master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| PtyError::Spawn(format!("resize: {e}")))?;
        *s.cols.lock().unwrap() = cols;
        *s.rows.lock().unwrap() = rows;
        Ok(())
    }

    pub fn kill(&self, id: &str, tenant: &TenantId) -> Result<(), PtyError> {
        let mut sessions = self.sessions.lock().unwrap();
        let s = sessions.get_mut(id).ok_or_else(|| PtyError::NoSuchSession(id.into()))?;
        if &s.tenant != tenant {
            return Err(PtyError::WrongTenant(id.into()));
        }
        // Polite first: the child gets a chance to save state and exit cleanly.
        s.killer.kill().map_err(|e| PtyError::Spawn(format!("kill: {e}")))?;

        // Then make sure the PIDs are actually released. A bare kill signals the
        // direct child only, so a child that ignores the signal never dies and
        // its own children — MCP servers, helper daemons — are orphaned to PID 1.
        // Escalation runs on a timer rather than inline: this must not block the
        // caller for the grace period.
        let pid = s.pid;
        if pid != 0 {
            std::thread::spawn(move || {
                std::thread::sleep(env::KILL_GRACE);
                if env::is_alive(pid) {
                    tracing::warn!(pid, "pty child ignored the polite kill — killing the tree");
                }
                // Run unconditionally: the leader may be gone while its
                // descendants still hold the group id, which is exactly the
                // orphan-reaping case.
                env::hard_kill_tree(pid);
            });
        }
        Ok(())
    }

    /// Sessions belonging to one tenant. A tenant can never enumerate another's,
    /// which is why this takes a `TenantId` rather than returning everything.
    pub fn list(&self, tenant: &TenantId) -> Vec<PtyInfo> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| &s.tenant == tenant)
            .map(|s| PtyInfo {
                id: s.id.clone(),
                cols: *s.cols.lock().unwrap(),
                rows: *s.rows.lock().unwrap(),
                cwd: s.cwd.clone(),
                command: s.command.clone(),
                pid: s.pid,
                last_output_at: *s.last_output_at.lock().unwrap(),
                has_output: *s.has_output.lock().unwrap(),
            })
            .collect()
    }

    /// Milliseconds since this session last emitted a byte, or `None` if it has
    /// not spoken yet. The hive's idle handshake reads this before typing.
    pub fn idle_for(&self, id: &str) -> Option<u64> {
        let sessions = self.sessions.lock().unwrap();
        let s = sessions.get(id)?;
        if !*s.has_output.lock().unwrap() { return None; }
        let last = *s.last_output_at.lock().unwrap();
        Some(now_ms().saturating_sub(last))
    }
}
