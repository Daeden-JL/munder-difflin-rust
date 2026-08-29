//! The hook server — the bridge between agent CLI lifecycle hooks and the floor.
//!
//! Each agent is launched with its hooks pointed at a shim that forwards the
//! payload to a Unix socket at `<hiveRoot>/hooks.sock`. One listener per tenant,
//! inside that tenant's own home: **the socket path is the authorization**, so
//! there is no token to check and no way to name another tenant's agent.
//!
//! Two things travel in opposite directions over one connection:
//!
//!   * **out** — every boundary is published to the tenant's WebSocket hub, so
//!     the UI reflects real activity instead of inferring it. This replaces the
//!     screen-scraping in `usePtyParser.ts`, whose own header calls itself "a
//!     stopgap until we wire real Claude Code hooks".
//!   * **back** — the reply is Claude Code's hook-return protocol, which is how
//!     the operator denies a tool, halts a session cleanly, or injects context.
//!     Decided inline with no round-trip, because the shim times out.
//!
//! Wire format: one JSON object, newline-terminated, then a JSON reply and the
//! connection closes. Deliberately not HTTP — the shim runs on every hook of
//! every agent, and a socket handshake is the cheaper thing to do thousands of
//! times.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use md_contract::{Push, ServerEvent};
use md_tenant::TenantId;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::control::Control;
use crate::hive::Hive;
use crate::ws::Hub;

/// A payload never grows past a few KB in practice; the cap stops a wedged or
/// hostile writer from buying unbounded memory with one connection.
const MAX_PAYLOAD: u64 = 1024 * 1024;

/// The fields the harness reads. Everything else in the payload is ignored
/// rather than rejected — the CLI adds fields between releases, and a strict
/// decoder would turn that into an outage.
#[derive(Debug, Default, Deserialize)]
pub struct HookPayload {
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    /// Set by an upstream Stop hook that already re-entered this boundary.
    #[serde(default)]
    pub stop_hook_active: bool,
    /// Status payloads only: the session's live context accounting.
    #[serde(default)]
    pub context_window: Option<ContextWindow>,
}

#[derive(Debug, Deserialize)]
pub struct ContextWindow {
    #[serde(default)]
    pub total_input_tokens: Option<u64>,
    /// The REAL window size — 200k vs 1M. Nothing else exposes this, which is
    /// why a status tick is worth handling at all.
    #[serde(default)]
    pub context_window_size: Option<u64>,
}

/// Everything one tenant's hook handling needs. Cloned per connection.
#[derive(Clone)]
pub struct HookCtx {
    pub tenant: TenantId,
    pub hive_root: PathBuf,
    pub hub: Hub,
    pub control: Arc<Control>,
}

/// Bind the tenant's socket and serve it until the process ends.
///
/// A stale socket file from a previous run is removed first: the file outlives
/// the process that made it, and `bind` on an existing path fails with EADDRINUSE
/// even though nothing is listening.
pub async fn serve(ctx: HookCtx) -> std::io::Result<()> {
    let path = ctx.hive_root.join("hooks.sock");
    std::fs::create_dir_all(&ctx.hive_root)?;
    let _ = std::fs::remove_file(&path);

    let listener = UnixListener::bind(&path)?;
    restrict(&path)?;
    tracing::info!(tenant = %ctx.tenant, path = %path.display(), "hook socket listening");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, &ctx).await {
                        tracing::debug!(tenant = %ctx.tenant, error = %e, "hook connection ended");
                    }
                });
            }
            // One failed accept must not take the listener down for the tenant.
            Err(e) => tracing::warn!(tenant = %ctx.tenant, error = %e, "hook accept failed"),
        }
    }
}

/// Owner-only. The socket is a control channel — whoever can write to it can
/// deny another agent's tools or halt its session.
///
/// This is correct for the Passthrough and Container backends, where the agent
/// runs as the same uid as the server. It is NOT yet correct for `LocalUid`,
/// where the agent runs as a different uid and would be locked out; that
/// backend needs a shared group and mode 0660, and is unimplemented besides.
fn restrict(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    let _ = path;
    Ok(())
}

async fn handle_conn(stream: UnixStream, ctx: &HookCtx) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    // Cap before buffering, so an unterminated write cannot buy memory by never
    // sending the newline this read is waiting for.
    BufReader::new(read.take(MAX_PAYLOAD)).read_line(&mut line).await?;

    // An unparseable payload still gets a well-formed reply. Returning nothing
    // would hang the shim, which blocks the agent's hook — a parse bug must not
    // become a stalled agent.
    let reply = match serde_json::from_str::<Value>(&line) {
        Ok(raw) => {
            let payload: HookPayload = serde_json::from_value(raw).unwrap_or_default();
            handle(&payload, ctx)
        }
        Err(e) => {
            tracing::debug!(tenant = %ctx.tenant, error = %e, "unparseable hook payload");
            json!({})
        }
    };

    write.write_all(reply.to_string().as_bytes()).await?;
    write.shutdown().await
}

/// Decide what to answer one hook boundary, and publish what happened.
///
/// Order matters and is load-bearing; see the comments at each branch.
pub fn handle(p: &HookPayload, ctx: &HookCtx) -> Value {
    let event = p.hook_event_name.as_deref().unwrap_or("Unknown");
    let agent = p.agent_id.as_deref().filter(|s| !s.is_empty());

    // Status is pure telemetry from the status-line shim, not a real boundary.
    // Handled FIRST and returned early so it can never trip the halt gate or
    // look like agent activity.
    if event == "Status" {
        if let (Some(id), Some(cw)) = (agent, p.context_window.as_ref()) {
            if let (Some(tokens), Some(limit)) = (cw.total_input_tokens, cw.context_window_size) {
                if limit > 0 {
                    ctx.hub.publish(
                        &ctx.tenant,
                        ServerEvent::new(
                            Push::HiveContextUpdate,
                            json!({ "agentId": id, "tokens": tokens, "limit": limit }),
                        ),
                    );
                }
            }
        }
        return json!({});
    }

    // A graceful halt overrides everything below: stop the agent CLEANLY at this
    // boundary rather than killing the PTY, so its session id stays resumable.
    if let Some(id) = agent {
        if ctx.control.should_halt(id) {
            emit(ctx, p, event, false);
            return json!({
                "continue": false,
                "stopReason": "Halted by the operator from the floor.",
            });
        }
    }

    // Capture the session id for an idempotent `--resume` and for cost dedup.
    if let (Some(id), Some(sid)) = (agent, p.session_id.as_deref()) {
        Hive::new(&ctx.hive_root).record_session(id, sid);
    }

    if matches!(event, "Stop" | "SubagentStop") {
        // Unread mail must NEVER become a forced continuation here: that path
        // bypasses the human-in-the-loop safety and can spend credits while a
        // person is mid-answer. Inbox files are durable; delivery happens later
        // through the guarded idle-only path.
        emit(ctx, p, event, false);
        return json!({});
    }

    // The fast, mechanical refusal: deny a gated or paused agent's tool call
    // inline. Slow human APPROVAL deliberately rides Claude's own permission
    // prompt instead — a round-trip here would hit the shim timeout.
    if event == "PreToolUse" {
        if let Some(id) = agent {
            let d = ctx.control.tool_decision(id, p.tool_name.as_deref().unwrap_or(""));
            if d.deny {
                let reason = d.reason.unwrap_or_else(|| "Denied by operator.".into());
                ctx.hub.publish(
                    &ctx.tenant,
                    ServerEvent::new(
                        Push::ControlApprovalRequest,
                        json!({ "agentId": id, "tool": p.tool_name, "reason": reason }),
                    ),
                );
                emit(ctx, p, event, true);
                return json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "deny",
                        "permissionDecisionReason": reason,
                    }
                });
            }
        }
    }

    // Mid-run steering: inject queued operator guidance as context rather than
    // typing into the TUI. Only ONE additionalContext may be returned per hook,
    // so anything else that wants one has to merge here rather than compete.
    if matches!(event, "UserPromptSubmit" | "PostToolUse") {
        if let Some(id) = agent {
            if let Some(steer) = ctx.control.take_steer(id) {
                emit(ctx, p, event, false);
                return json!({
                    "hookSpecificOutput": {
                        "hookEventName": event,
                        "additionalContext": steer,
                    }
                });
            }
        }
    }

    emit(ctx, p, event, false);
    json!({})
}

/// Publish one boundary to the tenant's hub.
///
/// The payload is validated before it leaves: `event` must be a non-empty
/// string and `agentId`, when present, must be non-empty too. The shim is a
/// separate process, so this is untrusted input crossing into the UI.
fn emit(ctx: &HookCtx, p: &HookPayload, event: &str, blocked: bool) {
    if event.is_empty() {
        return;
    }
    ctx.hub.publish(
        &ctx.tenant,
        ServerEvent::new(
            Push::HiveHookEvent,
            json!({
                "agentId": p.agent_id.as_deref().filter(|s| !s.is_empty()),
                "event": event,
                "tool": p.tool_name,
                "notificationType": p.notification_type,
                "source": p.source,
                "message": p.message,
                "blocked": blocked,
            }),
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::Receiver;

    fn ctx() -> (HookCtx, Receiver<ServerEvent>) {
        let tenant = TenantId::parse("dev").unwrap();
        let hub = Hub::new();
        let rx = hub.subscribe(&tenant);
        let root = std::env::temp_dir().join(format!("md-hooks-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        (
            HookCtx { tenant, hive_root: root, hub, control: Arc::new(Control::new()) },
            rx,
        )
    }

    fn payload(event: &str, agent: &str) -> HookPayload {
        HookPayload {
            hook_event_name: Some(event.into()),
            agent_id: Some(agent.into()),
            ..Default::default()
        }
    }

    #[test]
    fn a_paused_agent_is_denied_at_pretooluse() {
        let (c, mut rx) = ctx();
        c.control.pause("jim", true);
        let mut p = payload("PreToolUse", "jim");
        p.tool_name = Some("Bash".into());

        let out = handle(&p, &c);
        let d = &out["hookSpecificOutput"];
        assert_eq!(d["permissionDecision"], "deny");
        assert!(d["permissionDecisionReason"].as_str().unwrap().contains("Paused by operator"));

        // The approval request reaches the floor, and the boundary is marked
        // blocked so the UI can tell a denial from ordinary activity.
        assert_eq!(rx.try_recv().unwrap().channel, Push::ControlApprovalRequest.as_str());
        assert_eq!(rx.try_recv().unwrap().payload["blocked"], true);
    }

    #[test]
    fn an_ungated_tool_call_proceeds() {
        let (c, _rx) = ctx();
        let mut p = payload("PreToolUse", "jim");
        p.tool_name = Some("Read".into());
        assert_eq!(handle(&p, &c), json!({}));
    }

    /// Halt must win over every later branch, including the tool decision.
    #[test]
    fn halt_stops_cleanly_and_overrides_everything_below() {
        let (c, _rx) = ctx();
        c.control.halt("jim");
        let out = handle(&payload("PreToolUse", "jim"), &c);
        assert_eq!(out["continue"], false);
        assert!(out["stopReason"].as_str().unwrap().contains("Halted by the operator"));
    }

    /// A status tick is telemetry: it must not trip the halt gate, or a halted
    /// agent's status line would answer `continue: false` forever.
    #[test]
    fn a_status_tick_is_telemetry_not_a_boundary() {
        let (c, mut rx) = ctx();
        c.control.halt("jim");
        let mut p = payload("Status", "jim");
        p.context_window = Some(ContextWindow {
            total_input_tokens: Some(235_300),
            context_window_size: Some(1_000_000),
        });

        assert_eq!(handle(&p, &c), json!({}), "must not halt");
        let ev = rx.try_recv().unwrap();
        assert_eq!(ev.channel, Push::HiveContextUpdate.as_str());
        assert_eq!(ev.payload["limit"], 1_000_000);
        assert!(rx.try_recv().is_err(), "a status tick is not agent activity");
    }

    #[test]
    fn a_steer_rides_the_next_prompt_and_is_delivered_once() {
        let (c, _rx) = ctx();
        c.control.steer("jim", "prefer the smaller diff");

        let out = handle(&payload("UserPromptSubmit", "jim"), &c);
        assert_eq!(
            out["hookSpecificOutput"]["additionalContext"],
            "prefer the smaller diff"
        );
        assert_eq!(handle(&payload("UserPromptSubmit", "jim"), &c), json!({}));
    }

    /// Unread mail must not become a forced continuation at Stop — that path
    /// can spend credits while a person is mid-answer.
    #[test]
    fn stop_never_forces_a_continuation() {
        let (c, _rx) = ctx();
        assert_eq!(handle(&payload("Stop", "jim"), &c), json!({}));
        assert_eq!(handle(&payload("SubagentStop", "jim"), &c), json!({}));
    }

    /// The CLI adds payload fields between releases; an unknown one must be
    /// ignored rather than turned into an outage.
    #[test]
    fn unknown_payload_fields_are_ignored() {
        let raw = json!({
            "hook_event_name": "PostToolUse",
            "agent_id": "jim",
            "tool_name": "Edit",
            "some_field_from_a_future_release": { "nested": true },
        });
        let p: HookPayload = serde_json::from_value(raw).unwrap();
        assert_eq!(p.tool_name.as_deref(), Some("Edit"));
    }

    #[test]
    fn a_payload_with_no_agent_still_emits_the_boundary() {
        let (c, mut rx) = ctx();
        let p = HookPayload {
            hook_event_name: Some("SessionStart".into()),
            ..Default::default()
        };
        assert_eq!(handle(&p, &c), json!({}));
        let ev = rx.try_recv().unwrap();
        assert!(ev.payload["agentId"].is_null());
        assert_eq!(ev.payload["event"], "SessionStart");
    }

    /// End to end over a real socket: the reply must come back, or the shim
    /// hangs and the agent's hook blocks with it.
    #[tokio::test]
    async fn the_socket_answers_a_denial_over_the_wire() {
        let (c, _rx) = ctx();
        let root = std::env::temp_dir().join(format!("md-hooks-wire-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let c = HookCtx { hive_root: root.clone(), ..c };
        c.control.pause("jim", true);

        tokio::spawn(serve(c.clone()));
        // Give the listener a moment to bind before connecting.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut s = UnixStream::connect(root.join("hooks.sock")).await.unwrap();
        s.write_all(b"{\"hook_event_name\":\"PreToolUse\",\"agent_id\":\"jim\",\"tool_name\":\"Bash\"}\n")
            .await
            .unwrap();

        let mut reply = String::new();
        BufReader::new(s).read_line(&mut reply).await.unwrap();
        let v: Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    }

    /// A stale socket file outlives the process that made it; without the
    /// unlink, every restart fails to bind.
    #[tokio::test]
    async fn a_stale_socket_file_does_not_block_a_restart() {
        let root = std::env::temp_dir().join(format!("md-hooks-stale-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("hooks.sock"), b"stale").unwrap();

        let (c, _rx) = ctx();
        let c = HookCtx { hive_root: root, ..c };
        tokio::spawn(serve(c.clone()));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(UnixStream::connect(c.hive_root.join("hooks.sock")).await.is_ok());
    }
}
