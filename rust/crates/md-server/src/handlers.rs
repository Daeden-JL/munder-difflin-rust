//! Handler implementations for the ported channels.
//!
//! Arguments arrive positionally, matching the bridge they replace
//! (`invoke('pty:write', id, data)`). [`Ctx::arg`] does the extraction so a
//! missing or mistyped argument becomes a `BadArguments` response rather than a
//! panic that takes down the connection.

use std::path::{Path, PathBuf};

use md_contract::{ErrorCode, RpcResponse};
use md_tenant::{SpawnRequest, TenantId, TenantPaths};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

use crate::git;
use crate::hive;
use crate::spawn;
use crate::rpc::Op;
use crate::state::AppState;

pub struct Ctx {
    pub state: AppState,
    pub tenant: TenantId,
    pub paths: TenantPaths,
    pub args: Vec<Value>,
}

impl Ctx {
    fn arg<T: DeserializeOwned>(&self, i: usize) -> Result<T, RpcResponse> {
        let v = self.args.get(i).ok_or_else(|| {
            RpcResponse::err(ErrorCode::BadArguments, format!("missing argument {i}"))
        })?;
        serde_json::from_value(v.clone()).map_err(|e| {
            RpcResponse::err(ErrorCode::BadArguments, format!("argument {i}: {e}"))
        })
    }

    fn opt_arg<T: DeserializeOwned>(&self, i: usize) -> Option<T> {
        self.args.get(i).and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// Resolve a client-supplied path inside the tenant home.
    ///
    /// Every filesystem handler goes through this. The check is the server's own
    /// guard against a crafted argument — it does not constrain spawned agents,
    /// which the sandbox handles.
    fn resolve(&self, raw: &str) -> Result<PathBuf, RpcResponse> {
        let expanded = expand_tilde(raw, &self.paths.home());
        if !self.paths.contains(&expanded) {
            return Err(RpcResponse::err(
                ErrorCode::Forbidden,
                format!("path is outside tenant {}'s home", self.tenant),
            ));
        }
        Ok(expanded)
    }
}

/// `~` means the tenant's home, not the server user's. Getting this wrong would
/// point every tenant at the same directory.
fn expand_tilde(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        home.to_path_buf()
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home.join(rest)
    } else {
        let p = PathBuf::from(raw);
        if p.is_absolute() { p } else { home.join(p) }
    }
}

macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(resp) => return resp,
        }
    };
}

pub async fn run(op: Op, ctx: Ctx) -> RpcResponse {
    match op {
        Op::AppInfo => app_info(&ctx),
        Op::ConfigGet => config_get(&ctx),
        Op::ConfigUpdate => config_update(&ctx),
        Op::FsListDir => fs_list_dir(&ctx),
        Op::FsStatAbs => fs_stat(&ctx),
        Op::FsReadFile => fs_read_file(&ctx),
        Op::FsWriteFile => fs_write_file(&ctx),
        Op::PtySpawn => pty_spawn(&ctx),
        Op::PtyWrite => pty_write(&ctx),
        Op::PtyResize => pty_resize(&ctx),
        Op::PtyKill => pty_kill(&ctx),
        Op::PtyList => pty_list(&ctx),

        // The git plane is async: every one of these shells out, and blocking a
        // worker thread on `git log` would stall unrelated tenants.
        Op::GitIsRepo
        | Op::GitMainRepo
        | Op::GitBranch
        | Op::GitStatus
        | Op::GitLog
        | Op::GitLogGraph
        | Op::GitBranches
        | Op::GitAheadBehind
        | Op::GitDiff
        | Op::GitCommitFiles
        | Op::GitShowFile
        | Op::GitCompareRefs
        | Op::GitWorktrees
        | Op::GitCheckout => git_op(op, &ctx).await,

        Op::HiveRegistry
        | Op::HiveBoard
        | Op::HiveTasks
        | Op::HiveAddTask
        | Op::HivePatchTask
        | Op::HiveDeleteTask
        | Op::HiveLog
        | Op::HiveMemory
        | Op::HiveInbox
        | Op::HiveRenameAgent
        | Op::HivePatchAgentRole
        | Op::HiveSetArchived
        | Op::HiveSetAgentHold
        | Op::HiveSend => hive_op(op, &ctx),

        Op::ControlPause
        | Op::ControlResume
        | Op::ControlHalt
        | Op::ControlSteer
        | Op::ControlSnapshot
        | Op::ControlAutoDelivery
        | Op::ControlGateTool => control_op(op, &ctx),
    }
}

/// Each control channel answers with the agent's FULL snapshot rather than an
/// ack, so one round trip leaves the caller with the complete state — the
/// bridge promised `AgentControlSnapshot | null` and the UI renders from it.
fn control_op(op: Op, ctx: &Ctx) -> RpcResponse {
    let c = ctx.state.control(&ctx.tenant);
    let id: String = tri!(ctx.arg(0));
    RpcResponse::ok(match op {
        Op::ControlPause => c.pause(&id, tri!(ctx.arg::<bool>(1))),
        Op::ControlResume => c.resume(&id),
        Op::ControlHalt => c.halt(&id),
        Op::ControlSteer => c.steer(&id, &tri!(ctx.arg::<String>(1))),
        Op::ControlSnapshot => c.snapshot(&id),
        Op::ControlAutoDelivery => c.set_auto_delivery_paused(&id, tri!(ctx.arg::<bool>(1))),
        Op::ControlGateTool => c.gate_tool(&id, &tri!(ctx.arg::<String>(1)), tri!(ctx.arg::<bool>(2))),
        _ => unreachable!("control_op called with a non-control op"),
    })
}

/// The hive lives entirely inside the tenant's own harness home, so these need
/// no path argument and no containment check — the tenant IS the boundary.
fn hive_op(op: Op, ctx: &Ctx) -> RpcResponse {
    let h = hive::Hive::new(ctx.paths.hive_root());
    RpcResponse::ok(match op {
        Op::HiveRegistry => h.registry(),
        Op::HiveBoard => json!(h.board()),
        Op::HiveTasks => h.tasks(),
        // Matches the Electron default of 200 lines.
        Op::HiveLog => h.log_tail(ctx.opt_arg::<usize>(0).unwrap_or(200).min(5_000)),
        Op::HiveMemory => json!(h.memory(&tri!(ctx.arg::<String>(0)))),
        Op::HiveInbox => h.inbox(&tri!(ctx.arg::<String>(0))),
        Op::HiveAddTask => h.add_task(tri!(ctx.arg::<Value>(0))),
        Op::HivePatchTask => h.patch_task(&tri!(ctx.arg::<String>(0)), &tri!(ctx.arg::<Value>(1))),
        Op::HiveDeleteTask => h.delete_task(&tri!(ctx.arg::<String>(0))),
        Op::HiveRenameAgent => h.rename_agent(&tri!(ctx.arg::<String>(0)), &tri!(ctx.arg::<String>(1))),
        Op::HivePatchAgentRole => {
            h.patch_agent_role(&tri!(ctx.arg::<String>(0)), &tri!(ctx.arg::<String>(1)))
        }
        Op::HiveSetArchived => {
            let id: String = tri!(ctx.arg(0));
            let archived: bool = tri!(ctx.arg(1));
            let out = h.set_archived(&id, archived);
            // Announced only on a successful archive: the floor removes the desk
            // on this event, and firing it for a failed call would erase an agent
            // that is still there.
            if archived && out.get("ok").and_then(|v| v.as_bool()) == Some(true) {
                ctx.state.hub.publish(
                    &ctx.tenant,
                    md_contract::ServerEvent::new(
                        md_contract::Push::HiveAgentArchived,
                        json!({ "id": id }),
                    ),
                );
            }
            out
        }
        Op::HiveSetAgentHold => h.set_agent_hold(&tri!(ctx.arg::<String>(0)), tri!(ctx.arg::<bool>(1))),
        // `from` defaults to 'system'; the renderer passes 'human' for anything a
        // person dispatched, which is what the analytics counter keys on.
        Op::HiveSend => h.send(
            &tri!(ctx.arg::<Value>(0)),
            &ctx.opt_arg::<String>(1).unwrap_or_else(|| "system".into()),
        ),
        _ => unreachable!("hive_op called with a non-hive op"),
    })
}

/// Page size for the log channels: absent means the channel's default, present
/// is clamped to git's usable range rather than rejected — the Electron
/// handlers coerce rather than error, and the panels rely on that.
fn clamp_count(n: Option<i64>, default: i64) -> u32 {
    n.unwrap_or(default).clamp(1, 500) as u32
}

/// Every git channel takes the repo path as argument 0, and every one of them
/// must stay inside the tenant's home — so both checks live here once instead
/// of being repeated (and eventually forgotten) in fourteen handlers.
async fn git_op(op: Op, ctx: &Ctx) -> RpcResponse {
    let raw: String = tri!(ctx.arg(0));
    let cwd = tri!(ctx.resolve(&raw));

    // `git -C` on a non-directory reports a confusing error from deep inside
    // git; answering here keeps the failure legible.
    if !cwd.is_dir() {
        return RpcResponse::err(ErrorCode::BadArguments, format!("not a directory: {}", cwd.display()));
    }

    let v = match op {
        Op::GitIsRepo => git::is_repo(&cwd).await,
        Op::GitMainRepo => git::main_repo(&cwd).await,
        Op::GitBranch => git::branch(&cwd).await,
        Op::GitStatus => git::status(&cwd).await,
        Op::GitBranches => git::branches(&cwd).await,
        Op::GitAheadBehind => git::ahead_behind(&cwd).await,
        Op::GitWorktrees => git::worktrees(&cwd).await,
        // Defaults and the 1..=500 clamp both come from the Electron handlers.
        // The clamp is not cosmetic: `n` is client-supplied and reaches
        // `--max-count`, so an unclamped value is an invitation to walk the
        // whole history of a large repo on demand.
        Op::GitLog => git::log(&cwd, clamp_count(ctx.opt_arg::<i64>(1), 50)).await,
        Op::GitLogGraph => {
            git::log_graph(
                &cwd,
                clamp_count(ctx.opt_arg::<i64>(1), 200),
                ctx.opt_arg::<i64>(2).unwrap_or(0).max(0) as u32,
            )
            .await
        }
        Op::GitDiff => git::diff(&cwd, &tri!(ctx.arg::<String>(1))).await,
        Op::GitCommitFiles => git::commit_files(&cwd, &tri!(ctx.arg::<String>(1))).await,
        Op::GitShowFile => {
            git::show_file(&cwd, &tri!(ctx.arg::<String>(1)), &tri!(ctx.arg::<String>(2))).await
        }
        Op::GitCompareRefs => {
            // Mode defaults to the PR-style three-dot comparison, matching the bridge.
            let mode = ctx.opt_arg::<String>(3).unwrap_or_else(|| "three".into());
            git::compare_refs(&cwd, &tri!(ctx.arg::<String>(1)), &tri!(ctx.arg::<String>(2)), &mode).await
        }
        Op::GitCheckout => {
            git::checkout(&cwd, &tri!(ctx.arg::<String>(1)), ctx.opt_arg::<bool>(2).unwrap_or(false)).await
        }
        _ => unreachable!("git_op called with a non-git op"),
    };
    RpcResponse::ok(v)
}

fn app_info(ctx: &Ctx) -> RpcResponse {
    RpcResponse::ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "tenant": ctx.tenant.as_str(),
        "harnessHome": ctx.paths.harness_home(),
        "sandbox": ctx.state.sandbox.kind(),
        "platform": std::env::consts::OS,
    }))
}

fn config_get(ctx: &Ctx) -> RpcResponse {
    let path = ctx.paths.config_file();
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) => RpcResponse::ok(v),
            Err(e) => RpcResponse::err(ErrorCode::Internal, format!("config is not valid JSON: {e}")),
        },
        // A missing config is the first-run state, not an error: the Electron
        // version returns defaults here too.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RpcResponse::ok(json!({})),
        Err(e) => RpcResponse::err(ErrorCode::Internal, e.to_string()),
    }
}

fn config_update(ctx: &Ctx) -> RpcResponse {
    let patch: Value = tri!(ctx.arg(0));
    let path = ctx.paths.config_file();
    let mut current: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));

    // Shallow merge, matching the bridge's `Partial<HarnessConfig>` semantics.
    if let (Some(base), Some(p)) = (current.as_object_mut(), patch.as_object()) {
        for (k, v) in p { base.insert(k.clone(), v.clone()); }
    }

    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return RpcResponse::err(ErrorCode::Internal, e.to_string());
        }
    }
    match std::fs::write(&path, serde_json::to_vec_pretty(&current).unwrap_or_default()) {
        Ok(()) => RpcResponse::ok(current),
        Err(e) => RpcResponse::err(ErrorCode::Internal, e.to_string()),
    }
}

fn fs_list_dir(ctx: &Ctx) -> RpcResponse {
    let raw: String = tri!(ctx.arg(0));
    let dir = tri!(ctx.resolve(&raw));
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => return RpcResponse::err(ErrorCode::Internal, e.to_string()),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let meta = entry.metadata().ok();
        out.push(json!({
            "name": entry.file_name().to_string_lossy(),
            "path": entry.path(),
            "isDir": meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
            "size": meta.as_ref().map(|m| m.len()).unwrap_or(0),
        }));
    }
    // Directories first, then name — the order the file tree expects.
    out.sort_by(|a, b| {
        let ad = a["isDir"].as_bool().unwrap_or(false);
        let bd = b["isDir"].as_bool().unwrap_or(false);
        bd.cmp(&ad).then_with(|| {
            a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
        })
    });
    RpcResponse::ok(out)
}

fn fs_stat(ctx: &Ctx) -> RpcResponse {
    let raw: String = tri!(ctx.arg(0));
    let p = tri!(ctx.resolve(&raw));
    match std::fs::metadata(&p) {
        Ok(m) => RpcResponse::ok(json!({
            "path": p, "isDir": m.is_dir(), "isFile": m.is_file(), "size": m.len(),
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => RpcResponse::ok(Value::Null),
        Err(e) => RpcResponse::err(ErrorCode::Internal, e.to_string()),
    }
}

fn fs_read_file(ctx: &Ctx) -> RpcResponse {
    let raw: String = tri!(ctx.arg(0));
    let p = tri!(ctx.resolve(&raw));
    match std::fs::read_to_string(&p) {
        Ok(text) => RpcResponse::ok(json!({ "ok": true, "content": text })),
        Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

fn fs_write_file(ctx: &Ctx) -> RpcResponse {
    let raw: String = tri!(ctx.arg(0));
    let content: String = tri!(ctx.arg(1));
    let p = tri!(ctx.resolve(&raw));
    if let Some(dir) = p.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return RpcResponse::ok(json!({ "ok": false, "error": e.to_string() }));
        }
    }
    match std::fs::write(&p, content) {
        Ok(()) => RpcResponse::ok(json!({ "ok": true })),
        Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

/// Where the shim lives inside the agent's environment. Overridable because the
/// container path and a local dev build differ; the default matches the image.
fn hook_bin() -> String {
    std::env::var("MD_HOOK_BIN").unwrap_or_else(|_| "/usr/local/bin/md-hook".into())
}

fn pty_spawn(ctx: &Ctx) -> RpcResponse {
    let opts: Value = tri!(ctx.arg(0));
    let id = match opts.get("id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => return RpcResponse::err(ErrorCode::BadArguments, "spawn options need an `id`"),
    };
    let command = opts.get("command").and_then(|v| v.as_str()).unwrap_or("bash").to_string();
    let raw_cwd = opts.get("cwd").and_then(|v| v.as_str()).unwrap_or("~").to_string();
    let cwd = tri!(ctx.resolve(&raw_cwd));
    let mut args: Vec<String> = opts.get("args")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let cols = opts.get("cols").and_then(|v| v.as_u64()).unwrap_or(80) as u16;
    let rows = opts.get("rows").and_then(|v| v.as_u64()).unwrap_or(24) as u16;

    let mut env = std::collections::HashMap::new();
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("HOME".to_string(), ctx.paths.home().display().to_string());

    // `hive` present means "provision this as an agent, not a bare shell".
    // Provisioning runs BEFORE the spawn and its failure aborts it: an agent
    // that starts without its hooks looks live on the floor while reporting
    // nothing, which is worse than not starting.
    let hive_meta = opts.get("hive").cloned();
    if let Some(meta) = &hive_meta {
        let m = spawn::AgentMeta {
            id: id.clone(),
            name: meta.get("name").and_then(|v| v.as_str()).unwrap_or(&id).to_string(),
            provider: meta.get("provider").and_then(|v| v.as_str()).unwrap_or("claude").to_string(),
            role: meta.get("role").and_then(|v| v.as_str()).map(String::from),
            cwd: cwd.display().to_string(),
            is_god: meta.get("isGod").and_then(|v| v.as_bool()).unwrap_or(false),
        };
        let hive = hive::Hive::new(ctx.paths.hive_root());

        // Resume is resolved BEFORE provisioning, so a `requireResume` failure
        // leaves nothing behind. The lookup is unaffected by the order:
        // `sessionId` is written only by the hook server and merely PRESERVED by
        // provisioning, so it reads the same either way. (Electron provisions
        // first and so leaves a registry entry for an agent that never started.)
        let mut resume_args = Vec::new();
        if opts.get("resume").and_then(|v| v.as_bool()).unwrap_or(false) {
            match hive.registry()["agents"][&id]["sessionId"].as_str() {
                Some(sid) if !sid.is_empty() => {
                    resume_args.push("--resume".to_string());
                    resume_args.push(sid.to_string());
                }
                // `requireResume` exists so a caller that needs continuity fails
                // loudly instead of quietly starting a fresh thread.
                _ if opts.get("requireResume").and_then(|v| v.as_bool()).unwrap_or(false) => {
                    return RpcResponse::ok(
                        json!({ "ok": false, "error": "no recorded session to resume" }),
                    )
                }
                _ => {}
            }
        }

        let p = spawn::Provisioner {
            hive: &hive,
            hive_root: ctx.paths.hive_root(),
            hook_bin: hook_bin(),
        };
        let injection = match p.ensure_agent(&m) {
            Ok(i) => i,
            Err(e) => return RpcResponse::ok(json!({ "ok": false, "error": e })),
        };
        args.extend(resume_args);
        args.extend(injection.args);
        env.extend(injection.env);
    }

    let req = SpawnRequest {
        tenant: ctx.tenant.clone(),
        program: command.clone(),
        args, cwd: cwd.clone(), env, cols, rows,
    };

    let rx = match ctx.state.pty.spawn(&id, &req, &ctx.paths) {
        Ok(rx) => rx,
        Err(e) => return RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
    };

    // Announced only after the pty is actually up, so the floor never draws an
    // agent that failed to start.
    if let Some(meta) = hive_meta {
        let mut rec = meta.as_object().cloned().unwrap_or_default();
        rec.insert("id".into(), json!(id));
        rec.insert("cwd".into(), json!(cwd.display().to_string()));
        rec.insert("command".into(), json!(command));
        ctx.state.hub.publish(
            &ctx.tenant,
            md_contract::ServerEvent::new(md_contract::Push::HiveAgentSpawned, Value::Object(rec)),
        );
    }

    // Pump this session's frames onto the tenant's push channel. The task ends
    // when the pty closes its sender, so a dead session stops costing anything.
    let hub = ctx.state.hub.clone();
    let tenant = ctx.tenant.clone();
    let stream_id = id.clone();
    tokio::spawn(async move {
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(md_pty::PtyEvent::Data { data }) => hub.publish(
                    &tenant,
                    md_contract::ServerEvent::stream(
                        md_contract::Push::PtyData, &stream_id, json!(data)),
                ),
                Ok(md_pty::PtyEvent::Exit { exit_code, signal }) => {
                    hub.publish(&tenant, md_contract::ServerEvent::stream(
                        md_contract::Push::PtyExit, &stream_id,
                        json!({ "exitCode": exit_code, "signal": signal })));
                    break;
                }
                Ok(md_pty::PtyEvent::Relaunch) => hub.publish(
                    &tenant,
                    md_contract::ServerEvent::stream(
                        md_contract::Push::PtyRelaunch, &stream_id, Value::Null),
                ),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });

    RpcResponse::ok(json!({ "ok": true, "cwd": cwd }))
}

fn pty_write(ctx: &Ctx) -> RpcResponse {
    let id: String = tri!(ctx.arg(0));
    let data: String = tri!(ctx.arg(1));
    match ctx.state.pty.write(&id, &ctx.tenant, &data) {
        Ok(()) => RpcResponse::ok(json!({ "ok": true })),
        Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

fn pty_resize(ctx: &Ctx) -> RpcResponse {
    let id: String = tri!(ctx.arg(0));
    let cols: u16 = tri!(ctx.arg(1));
    let rows: u16 = tri!(ctx.arg(2));
    match ctx.state.pty.resize(&id, &ctx.tenant, cols, rows) {
        Ok(()) => RpcResponse::ok(json!({ "ok": true })),
        Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

fn pty_kill(ctx: &Ctx) -> RpcResponse {
    let id: String = tri!(ctx.arg(0));
    match ctx.state.pty.kill(&id, &ctx.tenant) {
        Ok(()) => RpcResponse::ok(json!({ "ok": true })),
        Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

fn pty_list(ctx: &Ctx) -> RpcResponse {
    let _ = ctx.opt_arg::<Value>(0);
    RpcResponse::ok(ctx.state.pty.list(&ctx.tenant))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_resolves_to_the_tenant_home_not_the_server_user() {
        let home = PathBuf::from("/srv/md/acme");
        assert_eq!(expand_tilde("~", &home), home);
        assert_eq!(expand_tilde("~/proj", &home), home.join("proj"));
        assert_eq!(expand_tilde("proj", &home), home.join("proj"));
        assert_eq!(expand_tilde("/etc/passwd", &home), PathBuf::from("/etc/passwd"));
    }
}
