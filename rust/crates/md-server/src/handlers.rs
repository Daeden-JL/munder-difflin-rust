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

        Op::AppStartClosingTime => closing_start(&ctx),
        Op::AppCancelClosingTime => closing_cancel(&ctx),

        Op::HiveTextSearch => {
            let q: String = tri!(ctx.arg(0));
            RpcResponse::ok(hive::Hive::new(ctx.paths.hive_root()).text_search(&q))
        }

        Op::HistoryAdd => {
            let e: Value = tri!(ctx.arg(0));
            let (Some(agent), Some(text)) = (
                e.get("agentId").and_then(|v| v.as_str()),
                e.get("text").and_then(|v| v.as_str()),
            ) else {
                return RpcResponse::ok(json!({ "ok": false, "error": "invalid args" }));
            };
            RpcResponse::ok(history(&ctx).add(agent, e.get("cwd").and_then(|v| v.as_str()), text))
        }
        Op::HistoryList => RpcResponse::ok(history(&ctx).list(
            ctx.opt_arg::<String>(0).as_deref().filter(|s| !s.is_empty()),
            ctx.opt_arg::<usize>(1).unwrap_or(200).min(1_000),
        )),
        Op::HistorySearch => RpcResponse::ok(history(&ctx).search(
            &tri!(ctx.arg::<String>(0)),
            ctx.opt_arg::<usize>(1).unwrap_or(50).min(1_000),
        )),

        Op::SessionResolveCwd => session_resolve_cwd(&ctx),
        Op::PtyRedraw => pty_redraw(&ctx),
        Op::FsReadBinary => fs_read_binary(&ctx),
        Op::ConfigEnsureHome => {
            // The tenant's harness home is provisioned at boot and is not the
            // client's to move, so this only confirms it exists.
            match std::fs::create_dir_all(ctx.paths.harness_home()) {
                Ok(()) => RpcResponse::ok(json!({ "ok": true })),
                Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
            }
        }
        Op::ConfigSetAgentTokenCap => config_set_token_cap(&ctx),
        // Counted, not stored. The Electron original feeds a telemetry pipeline
        // that has no equivalent here; the channel exists so the client's
        // fire-and-forget call is not an error.
        Op::AnalyticsMessageSent => {
            tracing::debug!(surface = ?ctx.opt_arg::<String>(0), tenant = %ctx.tenant, "message sent");
            RpcResponse::ok(Value::Null)
        }

        Op::KgList => RpcResponse::ok(kg(&ctx).list()),
        Op::KgStatus => RpcResponse::ok(kg(&ctx).status()),
        Op::KgGet => RpcResponse::ok(kg(&ctx).get(&tri!(ctx.arg::<String>(0)))),
        Op::KgRemove => RpcResponse::ok(kg(&ctx).remove(&tri!(ctx.arg::<String>(0)))),
        Op::KgSearch => RpcResponse::ok(kg(&ctx).search(
            &tri!(ctx.arg::<String>(0)),
            ctx.opt_arg::<usize>(1).unwrap_or(8),
        )),
        Op::KgIngestFiles => kg_ingest(&ctx),
        Op::RosterWrite => roster_write(&ctx),
        Op::MemoryReflectNow => memory_reflect(&ctx),

        Op::IntegrationsTemplates => RpcResponse::ok(crate::integrations::templates()),
        Op::IntegrationsList => RpcResponse::ok(integrations(&ctx).list()),
        Op::IntegrationsUpsert => {
            RpcResponse::ok(integrations(&ctx).upsert(&tri!(ctx.arg::<Value>(0))))
        }
        Op::IntegrationsRemove => {
            let req: Value = tri!(ctx.arg(0));
            RpcResponse::ok(integrations(&ctx).remove(req["id"].as_str().unwrap_or("")))
        }
        Op::IntegrationsSetSecret => {
            let req: Value = tri!(ctx.arg(0));
            RpcResponse::ok(integrations(&ctx).set_secret(
                req["id"].as_str().unwrap_or(""),
                req["secret"].as_str().unwrap_or(""),
            ))
        }
        Op::IntegrationsTest => integrations_test(&ctx).await,

        // Per-CLI BYOK keys. Write-only by the same rule as integration
        // secrets: `has` returns a boolean and nothing returns the value.
        Op::ProviderKeySet => {
            let req: Value = tri!(ctx.arg(0));
            let backend = req["backend"].as_str().unwrap_or("");
            let key = req["key"].as_str().unwrap_or("");
            if backend.is_empty() || key.is_empty() {
                return RpcResponse::ok(json!({ "ok": false, "error": "backend and key are required" }));
            }
            match crate::secrets::Secrets::new(&ctx.paths.harness_home())
                .set(&format!("provider:{backend}"), key)
            {
                Ok(()) => RpcResponse::ok(json!({ "ok": true })),
                Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
            }
        }
        Op::ProviderKeyHas => {
            let backend: String = tri!(ctx.arg(0));
            RpcResponse::ok(json!(crate::secrets::Secrets::new(&ctx.paths.harness_home())
                .has(&format!("provider:{backend}"))))
        }
        Op::ProviderKeyClear => {
            let backend: String = tri!(ctx.arg(0));
            match crate::secrets::Secrets::new(&ctx.paths.harness_home())
                .remove(&format!("provider:{backend}"))
            {
                Ok(_) => RpcResponse::ok(json!({ "ok": true })),
                Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
            }
        }

        Op::TriggersGetContext => {
            let cfg = read_config(&ctx);
            RpcResponse::ok(cfg.get("contextTrigger").cloned().unwrap_or_else(|| {
                json!({ "enabled": false, "thresholdPct": 80, "action": "notify" })
            }))
        }
        Op::GithubIssues => {
            let cwd = tri!(ctx.resolve(&tri!(ctx.arg::<String>(0))));
            RpcResponse::ok(crate::misc::github_issues(&cwd).await)
        }
        Op::GithubCiRuns => {
            let cwd = tri!(ctx.resolve(&tri!(ctx.arg::<String>(0))));
            RpcResponse::ok(crate::misc::github_ci_runs(&cwd).await)
        }
        Op::ToolsStatus => RpcResponse::ok(crate::misc::tools_status()),
        Op::SkillsLocal => {
            let cwd = tri!(ctx.resolve(&ctx.opt_arg::<String>(0).unwrap_or_else(|| "~".into())));
            RpcResponse::ok(crate::misc::local_skills(&ctx.paths.home(), &cwd))
        }
        Op::SkillsUninstall => {
            let path = tri!(ctx.resolve(&tri!(ctx.arg::<String>(0))));
            // Only ever a skill directory, and only inside the tenant. Without
            // the manifest check this would be an arbitrary recursive delete
            // reachable from the client.
            if !path.join("SKILL.md").is_file() {
                return RpcResponse::ok(json!({ "ok": false, "error": "not a skill directory" }));
            }
            match std::fs::remove_dir_all(&path) {
                Ok(()) => RpcResponse::ok(json!({ "ok": true })),
                Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
            }
        }

        Op::MissionsList => RpcResponse::ok(
            read_config(&ctx).get("missions").cloned().unwrap_or_else(|| json!([])),
        ),
        Op::MissionsSave => {
            let missions: Value = tri!(ctx.arg(0));
            let mut cfg = read_config(&ctx);
            cfg["missions"] = missions;
            match write_config(&ctx, &cfg) {
                Ok(()) => {
                    ctx.state.hub.publish(
                        &ctx.tenant,
                        md_contract::ServerEvent::new(md_contract::Push::MissionsUpdated, Value::Null),
                    );
                    RpcResponse::ok(json!({ "ok": true }))
                }
                Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
            }
        }
        Op::OrgGetTrigger => RpcResponse::ok(
            read_config(&ctx)
                .get("orgTrigger")
                .cloned()
                .unwrap_or_else(|| json!({ "enabled": false, "mode": "review" })),
        ),
        Op::OrgSetTrigger => {
            let next: Value = tri!(ctx.arg(0));
            let mut cfg = read_config(&ctx);
            cfg["orgTrigger"] = next.clone();
            match write_config(&ctx, &cfg) {
                Ok(()) => RpcResponse::ok(next),
                Err(e) => RpcResponse::err(ErrorCode::Internal, e.to_string()),
            }
        }

        // Workers are ephemeral agents: a live pty plus a registry record.
        // Derived rather than tracked separately, so the list cannot drift from
        // what is actually running.
        Op::WorkersList => {
            let reg = hive::Hive::new(ctx.paths.hive_root()).registry();
            let live: Vec<Value> = ctx
                .state
                .pty
                .list(&ctx.tenant)
                .into_iter()
                .filter(|s| reg["agents"][&s.id]["isGod"].as_bool() != Some(true))
                .map(|s| {
                    let a = &reg["agents"][&s.id];
                    json!({
                        "workerId": s.id, "name": a["name"].as_str().unwrap_or(&s.id),
                        "cwd": s.cwd, "pid": s.pid,
                        "idleMs": ctx.state.pty.idle_for(&s.id),
                        "onHold": a["onHold"].as_bool().unwrap_or(false),
                    })
                })
                .collect();
            let cap = read_config(&ctx)["defaultWorkerTokenCap"].as_i64().unwrap_or(0);
            RpcResponse::ok(json!({ "live": live, "preserved": [], "maxWorkers": cap }))
        }
        Op::WorkersStop => {
            let id: String = tri!(ctx.arg(0));
            match ctx.state.pty.kill(&id, &ctx.tenant) {
                Ok(()) => RpcResponse::ok(json!({ "ok": true })),
                Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
            }
        }

        Op::SlackSetConfig => slack_set_config(&ctx),
        Op::SlackStatus => {
            let base = std::env::var("MD_PUBLIC_ORIGIN").unwrap_or_default();
            let secrets = crate::secrets::Secrets::new(&ctx.paths.harness_home());
            RpcResponse::ok(json!({
                "running": true,
                "configured": secrets.has("slack:botToken") && secrets.has("slack:signingSecret"),
                "url": if base.is_empty() { Value::Null }
                       else { json!(format!("{}/hooks/{}/slack", base.trim_end_matches('/'), ctx.tenant)) },
            }))
        }
        Op::SlackReply => slack_reply(&ctx).await,

        Op::TelemetrySnapshot => RpcResponse::ok(telemetry_snapshot(&ctx)),
        Op::TelemetrySpans => {
            let id: String = tri!(ctx.arg(0));
            RpcResponse::ok(telemetry_snapshot(&ctx)["spans"][&id].clone())
        }
        Op::TelemetryUsage => {
            let id: String = tri!(ctx.arg(0));
            let snap = telemetry_snapshot(&ctx);
            let found = snap["usage"]
                .as_array()
                .and_then(|a| a.iter().find(|u| u["agentId"] == id.as_str()).cloned());
            RpcResponse::ok(found.unwrap_or(Value::Null))
        }

        Op::HiveAgentDirectory => {
            let reg = hive::Hive::new(ctx.paths.hive_root()).registry();
            let live: Vec<String> = ctx.state.pty.list(&ctx.tenant).into_iter().map(|s| s.id).collect();
            let agents: Vec<Value> = reg["agents"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(id, a)| {
                            let mut e = a.clone();
                            if let Some(o) = e.as_object_mut() {
                                o.insert("id".into(), json!(id));
                                // Liveness comes from the pty table, never the
                                // registry: a crashed agent keeps its record.
                                o.insert("live".into(), json!(live.contains(id)));
                            }
                            e
                        })
                        .collect()
                })
                .unwrap_or_default();
            RpcResponse::ok(json!({ "godId": reg["godId"], "agents": agents }))
        }
        Op::HiveAgentContext => {
            let id: String = tri!(ctx.arg(0));
            let snap = telemetry_snapshot(&ctx);
            RpcResponse::ok(
                snap["usage"]
                    .as_array()
                    .and_then(|a| a.iter().find(|u| u["agentId"] == id.as_str()))
                    .map(|u| u["contextTokens"].clone())
                    .unwrap_or(Value::Null),
            )
        }

        Op::WebhooksList => RpcResponse::ok(webhooks(&ctx).list()),
        Op::WebhooksSave => {
            let list: Vec<Value> = tri!(ctx.arg(0));
            RpcResponse::ok(webhooks(&ctx).save(&list))
        }
        Op::WebhooksDelete => RpcResponse::ok(webhooks(&ctx).delete(&tri!(ctx.arg::<String>(0)))),
        // Returned ONCE, here. It is never readable again: `list` reports only
        // whether a secret is set.
        Op::WebhooksGenerateSecret => RpcResponse::ok(json!(crate::webhooks::random_hex(24))),
        Op::WebhookGenerateSecret => {
            RpcResponse::ok(json!({ "ok": true, "secret": crate::webhooks::random_hex(24) }))
        }
        Op::WebhooksStatus | Op::WebhookStatus => {
            // Always running: the endpoints are routes on this server. The URL is
            // built from the configured public origin, because the server cannot
            // know how it is reached from outside.
            let base = std::env::var("MD_PUBLIC_ORIGIN").unwrap_or_default();
            RpcResponse::ok(json!({
                "running": true,
                "url": if base.is_empty() { Value::Null }
                       else { json!(format!("{}/hooks/{}", base.trim_end_matches('/'), ctx.tenant)) },
            }))
        }

        Op::TriggerHistoryList => RpcResponse::ok(trigger_history(&ctx)),
        Op::TriggerHistoryClear => {
            let source: String = tri!(ctx.arg(0));
            let kept: Vec<Value> = trigger_history(&ctx)
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .filter(|e| e["source"].as_str() != Some(source.as_str()))
                .collect();
            write_trigger_history(&ctx, kept);
            RpcResponse::ok(Value::Null)
        }
        Op::TriggerHistoryDecide => {
            let arg: Value = tri!(ctx.arg(0));
            let (id, decision) = (
                arg["id"].as_str().unwrap_or("").to_string(),
                arg["decision"].as_str().unwrap_or("").to_string(),
            );
            if !matches!(decision.as_str(), "approved" | "rejected") {
                return RpcResponse::ok(Value::Null);
            }
            let mut all: Vec<Value> = trigger_history(&ctx).as_array().cloned().unwrap_or_default();
            let Some(slot) = all.iter_mut().find(|e| e["id"].as_str() == Some(id.as_str())) else {
                return RpcResponse::ok(Value::Null);
            };
            // A decision is recorded once. Re-deciding would let an approved
            // request be replayed as a fresh one.
            if slot["decision"].is_string() {
                return RpcResponse::ok(slot.clone());
            }
            slot["decision"] = json!(decision);
            slot["decidedAt"] = json!(hive::iso_now());
            let updated = slot.clone();
            write_trigger_history(&ctx, all);
            ctx.state.hub.publish(
                &ctx.tenant,
                md_contract::ServerEvent::new(md_contract::Push::TriggerHistoryUpdated, Value::Null),
            );
            RpcResponse::ok(updated)
        }

        Op::TriggersSetContext => {
            let next: Value = tri!(ctx.arg(0));
            let mut cfg = read_config(&ctx);
            cfg["contextTrigger"] = next.clone();
            match write_config(&ctx, &cfg) {
                Ok(()) => RpcResponse::ok(next),
                Err(e) => RpcResponse::err(ErrorCode::Internal, e.to_string()),
            }
        }
    }
}

fn integrations(ctx: &Ctx) -> crate::integrations::Integrations {
    crate::integrations::Integrations::new(&ctx.paths.harness_home())
}

/// Slack credentials go to the encrypted store, never to config.json — the
/// signing secret and bot token are exactly the kind of value the fail-closed
/// store exists for.
fn slack_set_config(ctx: &Ctx) -> RpcResponse {
    let patch: Value = tri!(ctx.arg(0));
    let secrets = crate::secrets::Secrets::new(&ctx.paths.harness_home());
    for (field, reference) in [("signingSecret", "slack:signingSecret"), ("botToken", "slack:botToken")] {
        if let Some(v) = patch.get(field).and_then(|v| v.as_str()) {
            if v.is_empty() {
                let _ = secrets.remove(reference);
            } else if let Err(e) = secrets.set(reference, v) {
                return RpcResponse::ok(json!({ "ok": false, "error": e.to_string() }));
            }
        }
    }
    // Non-secret fields stay in config where the UI can read them back.
    let mut cfg = read_config(ctx);
    for field in ["defaultChannel", "enabled"] {
        if let Some(v) = patch.get(field) {
            cfg["slack"][field] = v.clone();
        }
    }
    match write_config(ctx, &cfg) {
        Ok(()) => RpcResponse::ok(json!({ "ok": true })),
        Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

/// Post a reply into a Slack thread. The bot token is read here and attached
/// here; it appears in no response and no log.
async fn slack_reply(ctx: &Ctx) -> RpcResponse {
    let m: Value = tri!(ctx.arg(0));
    let (channel, thread_ts, text) = (
        m["channel"].as_str().unwrap_or(""),
        m["thread_ts"].as_str().unwrap_or(""),
        m["text"].as_str().unwrap_or(""),
    );
    if channel.is_empty() || text.is_empty() {
        return RpcResponse::ok(json!({ "ok": false, "error": "channel and text are required" }));
    }
    let token = match crate::secrets::Secrets::new(&ctx.paths.harness_home()).get("slack:botToken") {
        Ok(Some(t)) => t,
        Ok(None) => return RpcResponse::ok(json!({ "ok": false, "error": "no Slack bot token configured" })),
        Err(e) => return RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
    };

    let mut body = json!({ "channel": channel, "text": text });
    if !thread_ts.is_empty() {
        body["thread_ts"] = json!(thread_ts);
    }
    let res = reqwest::Client::new()
        .post("https://slack.com/api/chat.postMessage")
        .bearer_auth(token)
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    match res {
        Ok(r) => match r.json::<Value>().await {
            // Slack answers 200 with {ok:false,error} for application errors, so
            // the status code alone is not the answer.
            Ok(v) => RpcResponse::ok(json!({
                "ok": v["ok"].as_bool().unwrap_or(false),
                "error": v.get("error").cloned().unwrap_or(Value::Null),
            })),
            Err(_) => RpcResponse::ok(json!({ "ok": false, "error": "unreadable Slack response" })),
        },
        Err(_) => RpcResponse::ok(json!({ "ok": false, "error": "request failed" })),
    }
}

/// Per-agent usage and tool spans, derived from the session transcripts.
///
/// Derived on read rather than accumulated in memory: the transcripts are the
/// record, and a separate counter would drift from them across a restart.
fn telemetry_snapshot(ctx: &Ctx) -> Value {
    let reg = hive::Hive::new(ctx.paths.hive_root()).registry();
    let mut usage = Vec::new();
    let mut spans = serde_json::Map::new();

    if let Some(agents) = reg["agents"].as_object() {
        for (id, a) in agents {
            let (Some(cwd), Some(session)) = (
                a["cwd"].as_str(),
                a["sessionId"].as_str().filter(|s| !s.is_empty()),
            ) else {
                continue;
            };
            let Some(file) = crate::transcript::session_file(&ctx.paths.home(), cwd, session) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&file) else { continue };

            let (mut input, mut output, mut cache_read, mut cache_write) = (0u64, 0u64, 0u64, 0u64);
            let mut model = Value::Null;
            let mut context_tokens = 0u64;
            let mut tool_spans = Vec::new();

            for line in text.lines() {
                let Ok(rec) = serde_json::from_str::<Value>(line) else { continue };
                if rec["type"] != "assistant" {
                    continue;
                }
                if let Some(m) = rec["message"]["model"].as_str() {
                    model = json!(m);
                }
                let u = &rec["message"]["usage"];
                let n = |k: &str| u[k].as_u64().unwrap_or(0);
                input += n("input_tokens");
                output += n("output_tokens");
                cache_write += n("cache_creation_input_tokens");
                cache_read += n("cache_read_input_tokens");
                // The LAST assistant record's totals are the live context
                // occupancy; the running sums above are lifetime spend.
                let live = n("input_tokens") + n("output_tokens")
                    + n("cache_creation_input_tokens") + n("cache_read_input_tokens");
                if live > 0 {
                    context_tokens = live;
                }
                if let Some(blocks) = rec["message"]["content"].as_array() {
                    for b in blocks.iter().filter(|b| b["type"] == "tool_use") {
                        tool_spans.push(json!({ "tool": b["name"], "ts": rec["timestamp"] }));
                    }
                }
            }

            // Only the tail is useful to a UI, and an agent can accumulate
            // thousands.
            let start = tool_spans.len().saturating_sub(100);
            spans.insert(id.clone(), json!(tool_spans[start..]));
            usage.push(json!({
                "agentId": id, "model": model,
                "inputTokens": input, "outputTokens": output,
                "cacheReadTokens": cache_read, "cacheWriteTokens": cache_write,
                "contextTokens": context_tokens,
            }));
        }
    }
    json!({ "usage": usage, "spans": spans })
}

fn webhooks(ctx: &Ctx) -> crate::webhooks::Webhooks {
    crate::webhooks::Webhooks::new(&ctx.paths.harness_home())
}

fn trigger_history_path(ctx: &Ctx) -> std::path::PathBuf {
    ctx.paths.harness_home().join("trigger-history.json")
}

fn trigger_history(ctx: &Ctx) -> Value {
    std::fs::read_to_string(trigger_history_path(ctx))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(|v: &Value| v.is_array())
        .unwrap_or_else(|| json!([]))
}

fn write_trigger_history(ctx: &Ctx, entries: Vec<Value>) {
    let path = trigger_history_path(ctx);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serde_json::to_vec_pretty(&json!(entries)).unwrap_or_default());
}

fn read_config(ctx: &Ctx) -> Value {
    std::fs::read_to_string(ctx.paths.config_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn write_config(ctx: &Ctx, cfg: &Value) -> std::io::Result<()> {
    let path = ctx.paths.config_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(cfg).unwrap_or_default())
}

/// Make the integration's own test request.
///
/// The credential is attached by the registry and never leaves this process in
/// any other direction — the response carries a status code, not a body, so a
/// service that echoes its own auth header cannot leak it back to the client.
async fn integrations_test(ctx: &Ctx) -> RpcResponse {
    let req: Value = tri!(ctx.arg(0));
    let id = req["id"].as_str().unwrap_or("");
    let path = req["path"].as_str();

    let (url, headers) = match integrations(ctx).test_request(id, path) {
        Ok(v) => v,
        Err(e) => return RpcResponse::ok(json!({ "ok": false, "error": e })),
    };

    let mut builder = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        // No redirects: a redirect could move the request to another host and
        // carry the credential with it.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map(|c| c.get(&url))
        .unwrap_or_else(|_| reqwest::Client::new().get(&url));
    for (k, v) in headers {
        builder = builder.header(k, v);
    }

    match builder.send().await {
        Ok(res) => {
            let status = res.status().as_u16();
            RpcResponse::ok(json!({ "ok": res.status().is_success(), "status": status }))
        }
        // The error is stringified from the transport, which never contains a
        // header value — but the URL could contain a query the caller supplied,
        // so only the class of failure is reported.
        Err(e) => RpcResponse::ok(json!({
            "ok": false,
            "error": if e.is_timeout() { "timed out" } else { "request failed" },
        })),
    }
}

fn kg(ctx: &Ctx) -> crate::knowledge::Knowledge {
    crate::knowledge::Knowledge::new(ctx.paths.harness_home().join("knowledge"))
}

/// Ingest files the tenant already has on the server.
///
/// Every path is resolved through the tenant guard: this is the one channel that
/// reads arbitrary files at the client's request, so a traversal here would read
/// the server's own filesystem into a searchable store.
fn kg_ingest(ctx: &Ctx) -> RpcResponse {
    let paths: Vec<String> = tri!(ctx.arg(0));
    let tags: Vec<String> = ctx.opt_arg(1).unwrap_or_default();
    let store = kg(ctx);

    let mut results = Vec::new();
    for raw in paths {
        let resolved = match ctx.resolve(&raw) {
            Ok(p) => p,
            Err(_) => {
                results.push(json!({ "ok": false, "srcPath": raw, "error": "outside the tenant home" }));
                continue;
            }
        };
        // Read as text. A binary file would be indexed as mojibake, so it is
        // refused rather than silently producing an unsearchable document.
        match std::fs::read(&resolved) {
            Ok(bytes) if bytes.contains(&0) => {
                results.push(json!({ "ok": false, "srcPath": raw, "error": "binary file" }));
            }
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let title = resolved
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("untitled");
                results.push(store.ingest(title, &raw, &text, &tags));
            }
            Err(e) => results.push(json!({ "ok": false, "srcPath": raw, "error": e.to_string() })),
        }
    }
    let ok = results.iter().all(|r| r["ok"] == true);
    RpcResponse::ok(json!({ "ok": ok, "results": results }))
}

/// The UI floor roster — desks, characters, positions. Distinct from the hive
/// registry, which is agent identity; this is presentation and belongs to the
/// client, so it is stored verbatim rather than validated field by field.
fn roster_write(ctx: &Ctx) -> RpcResponse {
    let snap: Value = tri!(ctx.arg(0));
    if !snap.is_object() {
        return RpcResponse::ok(json!({ "ok": false, "skipped": "not an object" }));
    }
    let path = ctx.paths.roster_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Write-then-rename: the roster is read at boot, and a truncated file would
    // present as an empty floor.
    let tmp = path.with_extension("json.tmp");
    match std::fs::write(&tmp, serde_json::to_vec_pretty(&snap).unwrap_or_default())
        .and_then(|()| std::fs::rename(&tmp, &path))
    {
        Ok(()) => RpcResponse::ok(json!({ "ok": true })),
        Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

/// Ask an agent to condense its own memory.
///
/// Condensation is the AGENT's work, not the harness's — it is the one that
/// knows which of its notes still matter. This delivers the instruction and
/// returns what was asked, rather than rewriting `memory.md` behind its back.
fn memory_reflect(ctx: &Ctx) -> RpcResponse {
    let id: String = tri!(ctx.arg(0));
    let h = hive::Hive::new(ctx.paths.hive_root());
    if h.registry()["agents"][&id].is_null() {
        return RpcResponse::ok(json!([]));
    }
    let out = h.send(
        &json!({
            "to": id,
            "act": "request",
            "subject": "Condense your memory",
            "body": "Re-read your memory.md and rewrite it: keep durable facts, decisions and                      context; drop anything superseded, transient, or already visible in the                      repository. Keep it shorter than you found it.",
        }),
        "human",
    );
    let delivered = out["delivered"].as_array().is_some_and(|a| !a.is_empty());
    RpcResponse::ok(json!([{ "id": id, "condensed": delivered }]))
}

fn history(ctx: &Ctx) -> crate::history::History {
    crate::history::History::new(&ctx.paths.harness_home())
}

/// Which working directory a recorded session belongs to.
///
/// Transcripts are filed under a directory named for the cwd that produced
/// them, so the answer is found by looking for the session's file rather than
/// by storing a mapping that could drift.
fn session_resolve_cwd(ctx: &Ctx) -> RpcResponse {
    let session: String = tri!(ctx.arg(0));
    if session.is_empty()
        || !session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return RpcResponse::ok(Value::Null);
    }
    // The registry is authoritative and cheap, so try it before walking a
    // directory tree.
    let reg = hive::Hive::new(ctx.paths.hive_root()).registry();
    if let Some(agents) = reg["agents"].as_object() {
        for a in agents.values() {
            if a["sessionId"].as_str() == Some(session.as_str()) {
                if let Some(cwd) = a["cwd"].as_str() {
                    return RpcResponse::ok(json!(cwd));
                }
            }
        }
    }
    // Otherwise find the transcript and read the cwd the CLI recorded in it.
    let projects = ctx.paths.home().join(".claude/projects");
    let Ok(dirs) = std::fs::read_dir(&projects) else { return RpcResponse::ok(Value::Null) };
    for d in dirs.filter_map(Result::ok) {
        let file = d.path().join(format!("{session}.jsonl"));
        if !file.is_file() {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&file) {
            for line in text.lines().take(50) {
                if let Ok(rec) = serde_json::from_str::<Value>(line) {
                    if let Some(cwd) = rec.get("cwd").and_then(|v| v.as_str()) {
                        return RpcResponse::ok(json!(cwd));
                    }
                }
            }
        }
    }
    RpcResponse::ok(Value::Null)
}

/// Nudge a TUI into repainting, by resizing a column narrower and back.
///
/// There is no portable "redraw" signal; a size change is what every terminal
/// program already listens for. Kept because an agent CLI that was resized
/// while hidden can otherwise sit with a corrupted frame.
fn pty_redraw(ctx: &Ctx) -> RpcResponse {
    let id: String = tri!(ctx.arg(0));
    let Some(info) = ctx.state.pty.list(&ctx.tenant).into_iter().find(|s| s.id == id) else {
        return RpcResponse::ok(json!({ "ok": false, "error": "no such session" }));
    };
    // Resize narrower and back. Programs redraw on SIGWINCH, and the round trip
    // leaves the terminal exactly as it was.
    let (cols, rows) = (info.cols.max(2), info.rows);
    if let Err(e) = ctx.state.pty.resize(&id, &ctx.tenant, cols - 1, rows) {
        return RpcResponse::ok(json!({ "ok": false, "error": e.to_string() }));
    }
    match ctx.state.pty.resize(&id, &ctx.tenant, cols, rows) {
        Ok(()) => RpcResponse::ok(json!({ "ok": true })),
        Err(e) => RpcResponse::ok(json!({ "ok": false, "error": e.to_string() })),
    }
}

/// Read a file as bytes, base64-encoded.
///
/// Separate from `fs:readFile` because the caller wants an image or an archive,
/// where a lossy UTF-8 conversion would silently corrupt the content.
fn fs_read_binary(ctx: &Ctx) -> RpcResponse {
    let root: String = tri!(ctx.arg(0));
    let rel: String = tri!(ctx.arg(1));
    let base = tri!(ctx.resolve(&root));
    let full = tri!(ctx.resolve(&base.join(&rel).display().to_string()));
    if !full.starts_with(&base) {
        return RpcResponse::err(ErrorCode::Forbidden, "path escapes the given root");
    }
    match std::fs::read(&full) {
        Ok(bytes) => RpcResponse::ok(json!(base64(&bytes))),
        Err(e) => RpcResponse::err(ErrorCode::Internal, e.to_string()),
    }
}

/// Minimal base64. A dependency for one call site that runs rarely would be
/// more code to audit than the twenty lines it replaces.
fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Per-agent token budget, stored on the tenant's config.
fn config_set_token_cap(ctx: &Ctx) -> RpcResponse {
    let agent: String = tri!(ctx.arg(0));
    let cap: i64 = tri!(ctx.arg(1));
    let path = ctx.paths.config_file();
    let mut cfg: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    if !cfg.is_object() {
        cfg = json!({});
    }
    let caps = cfg
        .as_object_mut()
        .unwrap()
        .entry("agentTokenCaps")
        .or_insert_with(|| json!({}));
    if let Some(m) = caps.as_object_mut() {
        // A non-positive cap clears it rather than pinning the agent at zero,
        // which would look like a budget of nothing rather than no budget.
        if cap > 0 {
            m.insert(agent, json!(cap));
        } else {
            m.remove(&agent);
        }
    }
    match std::fs::write(&path, serde_json::to_vec_pretty(&cfg).unwrap_or_default()) {
        Ok(()) => RpcResponse::ok(cfg),
        Err(e) => RpcResponse::err(ErrorCode::Internal, e.to_string()),
    }
}

/// Agents with a PTY right now. The registry is not a substitute: an agent that
/// died with a crash keeps its record without ever being archived, so a
/// registry-based roster waits forever on something that can never answer.
fn live_agents(ctx: &Ctx) -> Vec<String> {
    ctx.state
        .pty
        .list(&ctx.tenant)
        .into_iter()
        .map(|s| s.id)
        .collect()
}

fn publish_progress(ctx: &Ctx, p: &crate::closing::Progress) {
    ctx.state.hub.publish(
        &ctx.tenant,
        md_contract::ServerEvent::new(md_contract::Push::AppClosingTime, p),
    );
}

/// Announce one routed message to the floor, and let closing time see it.
///
/// Shared by `hive:send` and the outbox router, so a message routed by an agent
/// counts toward the shutdown exactly as one sent through the API does.
pub fn announce_routed(
    state: &AppState,
    tenant: &TenantId,
    paths: &TenantPaths,
    msg: &Value,
    delivered: &[String],
) {
    let to = msg.get("to").and_then(|v| v.as_str()).unwrap_or("");
    state.hub.publish(
        tenant,
        md_contract::ServerEvent::new(
            md_contract::Push::HiveMessage,
            json!({
                "id": msg.get("id"),
                "from": msg.get("from"),
                "to": to,
                "act": msg.get("act"),
                "subject": msg.get("subject"),
                "targets": delivered,
                // Tints the floor envelope for mail the agent flagged for the
                // human (now routed to the god proxy). Cosmetic — there is no
                // approval queue behind it.
                "needsHuman": to == "human",
            }),
        ),
    );

    let hive = hive::Hive::new(paths.hive_root());
    let live: Vec<String> = state.pty.list(tenant).into_iter().map(|s| s.id).collect();
    if let Some((progress, action)) =
        state.closing(tenant).on_routed(msg, delivered, &hive.registry(), &live)
    {
        apply_action(state, tenant, paths, action);
        state.hub.publish(
            tenant,
            md_contract::ServerEvent::new(md_contract::Push::AppClosingTime, &progress),
        );
    }
}

/// Perform whatever the controller decided. Kept here rather than inside
/// `closing` so that module stays testable without a hive or a pty manager.
fn apply(ctx: &Ctx, action: crate::closing::Action) {
    apply_action(&ctx.state, &ctx.tenant, &ctx.paths, action)
}

fn apply_action(
    state: &AppState,
    tenant: &TenantId,
    paths: &TenantPaths,
    action: crate::closing::Action,
) {
    use crate::closing::Action;
    match action {
        Action::None => {}
        // Sent as the human: closing time is the human's instruction, and the
        // god's inbox should show it as such.
        Action::Tell(msg) => {
            hive::Hive::new(paths.hive_root()).send(&msg, "human");
        }
        Action::Conclude => {
            let state = state.clone();
            let tenant = tenant.clone();
            let closing = state.closing(&tenant);
            let generation = closing.generation();
            tokio::spawn(async move {
                // The grace lets the god's final commit and log writes land, so
                // the floor visibly concludes instead of vanishing mid-sentence.
                tokio::time::sleep(crate::closing::Closing::teardown_grace()).await;
                if !closing.finish(generation) {
                    return; // cancelled or superseded while we waited
                }
                // This tenant's floor only. In Electron the protocol ended in
                // app.quit(); here that would take every other tenant down too.
                for s in state.pty.list(&tenant) {
                    let _ = state.pty.kill(&s.id, &tenant);
                }
                tracing::info!(%tenant, "closing time complete — floor wound down");
            });
        }
    }
}

fn closing_start(ctx: &Ctx) -> RpcResponse {
    let hive = hive::Hive::new(ctx.paths.hive_root());
    let closing = ctx.state.closing(&ctx.tenant);
    let started = match closing.start(&hive.registry(), &live_agents(ctx)) {
        Ok(v) => v,
        // A refusal is a value, not a transport error: the UI falls back to a
        // hard close and needs the reason to say why.
        Err(e) => return RpcResponse::ok(json!({ "ok": false, "error": e })),
    };

    // Steer notes reach agents that are deeply busy: the inbox brief only lands
    // when one next stops, so a worker hours into a task would hold the whole
    // shutdown.
    let control = ctx.state.control(&ctx.tenant);
    for s in started.steers {
        control.steer(&s.agent, &s.note);
    }
    apply(ctx, started.action);
    publish_progress(ctx, &started.progress);

    // Arm the "taking a while" notice. It reports; it never tears anything down.
    let generation = closing.generation();
    let (state, tenant) = (ctx.state.clone(), ctx.tenant.clone());
    tokio::spawn(async move {
        tokio::time::sleep(crate::closing::Closing::timeout()).await;
        if let Some(p) = state.closing(&tenant).timed_out(generation) {
            state.hub.publish(
                &tenant,
                md_contract::ServerEvent::new(md_contract::Push::AppClosingTime, p),
            );
        }
    });

    RpcResponse::ok(json!({ "ok": true }))
}

fn closing_cancel(ctx: &Ctx) -> RpcResponse {
    let Some(cancelled) = ctx.state.closing(&ctx.tenant).cancel() else {
        return RpcResponse::ok(json!({ "ok": true }));
    };
    // Drop steers no hook boundary has consumed yet, so a busy agent is not
    // told to shut down after the human cancelled.
    let control = ctx.state.control(&ctx.tenant);
    for id in cancelled.clear {
        control.clear_steers(&id);
    }
    apply(ctx, cancelled.action);
    publish_progress(ctx, &cancelled.progress);
    RpcResponse::ok(json!({ "ok": true }))
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
        Op::HiveSend => {
            let out = h.send(
                &tri!(ctx.arg::<Value>(0)),
                &ctx.opt_arg::<String>(1).unwrap_or_else(|| "system".into()),
            );
            // Closing time watches routed traffic for ACKs and the god's
            // conclusion. It reads `delivered`, not the intended recipient: an
            // ACK that never reached the god has not happened.
            let delivered: Vec<String> = out["delivered"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            announce_routed(&ctx.state, &ctx.tenant, &ctx.paths, &out["message"], &delivered);
            out
        }
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
