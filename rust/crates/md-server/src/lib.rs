//! Munder Difflin server: the Rust replacement for the Electron main process.

pub mod accounts;
pub mod auth;
pub mod closing;
pub mod control;
pub mod git;
pub mod hooks;
pub mod history;
pub mod hive;
pub mod integrations;
pub mod knowledge;
pub mod handlers;
pub mod engines;
pub mod mcp;
pub mod misc;
pub mod realtime;
pub mod rpc;
pub mod secrets;
pub mod spawn;
pub mod state;
pub mod transcript;
pub mod webhooks;
pub mod ws;

use std::collections::HashMap;
use std::sync::Arc;

use axum::routing::{get, post};
use axum::{Json, Router};
use md_pty::PtyManager;
use md_tenant::{Sandbox, TenantId, TenantPaths};

use auth::SessionStore;
use state::{AppState, ServerConfig};
use ws::Hub;

/// Start a tenant's background services: the hook socket and the outbox router.
///
/// Called at startup for every known tenant, and again when an admin creates an
/// account in a NEW one — the same path both times, so a tenant created at
/// runtime is not a second-class one missing half its machinery.
fn spawn_tenant_services(state: &AppState, tenant: &TenantId) {
    let Some(paths) = state.paths(tenant) else { return };

    // The hook socket lives inside the tenant's own hive directory, so the path
    // is the authorization and there is nothing further to check on a
    // connection.
    let ctx = hooks::HookCtx {
        tenant: tenant.clone(),
        hive_root: paths.hive_root(),
        hub: state.hub.clone(),
        control: state.control(tenant),
    };
    tokio::spawn(async move {
        // A tenant whose socket cannot bind loses hook-driven UI updates, but
        // the rest of its server keeps working — so this logs rather than
        // aborting startup for everyone.
        if let Err(e) = hooks::serve(ctx.clone()).await {
            tracing::error!(tenant = %ctx.tenant, error = %e, "hook socket unavailable");
        }
    });

    // The outbox router. Polling rather than filesystem watching: agents write
    // these files by hand from arbitrary processes, and a poll is cheap and does
    // not depend on platform watch semantics.
    let (state, tenant, paths) = (state.clone(), tenant.clone(), paths.clone());
    tokio::spawn(async move {
        let hive = hive::Hive::new(paths.hive_root());
        let mut tick = tokio::time::interval(ROUTER_INTERVAL);
        loop {
            tick.tick().await;
            // Blocking file IO, so it does not belong on the async worker.
            let hive = hive.clone();
            let Ok(routed) = tokio::task::spawn_blocking(move || hive.route_once()).await else {
                continue;
            };
            for r in routed {
                // A message addressed to the floor itself is a request to
                // change it, not mail — the router deliberately delivered it to
                // nobody and left acting on it to here, where the tenant's
                // config is in reach.
                if r.message["to"] == hive::HARNESS {
                    handlers::harness_request(&state, &tenant, &paths, &r.message);
                    continue;
                }
                handlers::announce_routed(&state, &tenant, &paths, &r.message, &r.delivered);
            }
        }
    });
}

/// How often each tenant's outboxes are swept. Matches the Electron original:
/// fast enough that agent-to-agent mail feels immediate, slow enough that an
/// idle floor costs nothing.
const ROUTER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1500);

/// Build the app. Kept separate from `main` so tests can drive the real router
/// rather than a stand-in that drifts from it.
///
/// `async` because it spawns one hook listener per tenant and so needs a
/// runtime. That is worth stating in the signature: a sync version would compile
/// everywhere and then panic at startup when called from the wrong place.
pub async fn build(cfg: &ServerConfig, accounts: Arc<accounts::Accounts>, sandbox: Arc<dyn Sandbox>)
    -> anyhow::Result<Router>
{
    let tenants: HashMap<TenantId, TenantPaths> = accounts
        .tenants()
        .into_iter()
        .map(|t| (t.clone(), TenantPaths::new(&cfg.data_root, t)))
        .collect();

    // Refuse to start rather than discovering the misconfiguration at first
    // spawn — on passthrough that discovery would be a cross-tenant leak.
    sandbox.preflight(tenants.len())?;

    for paths in tenants.values() {
        std::fs::create_dir_all(paths.harness_home())?;
        std::fs::create_dir_all(paths.workspaces())?;
    }

    let control: HashMap<TenantId, Arc<control::Control>> = tenants
        .keys()
        .map(|t| (t.clone(), Arc::new(control::Control::new())))
        .collect();
    let closing: HashMap<TenantId, Arc<closing::Closing>> = tenants
        .keys()
        .map(|t| (t.clone(), Arc::new(closing::Closing::new())))
        .collect();
    let realtime: HashMap<TenantId, Arc<realtime::Realtime>> = tenants
        .keys()
        .map(|t| (t.clone(), Arc::new(realtime::Realtime::new())))
        .collect();

    let state = AppState {
        sessions: SessionStore::new(),
        accounts,
        data_root: cfg.data_root.clone(),
        tenants: Arc::new(std::sync::RwLock::new(tenants)),
        pty: Arc::new(PtyManager::new(Arc::clone(&sandbox))),
        hub: Hub::new(),
        sandbox,
        control: Arc::new(control),
        closing: Arc::new(closing),
        realtime: Arc::new(realtime),
    };

    for tenant in state.tenant_ids() {
        spawn_tenant_services(&state, &tenant);
    }

    let mut app = Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/health", get(health))
        .route("/api/me", get(me))
        .route("/api/mcp", get(mcp_catalog))
        .route("/api/engines", get(engine_catalog))
        .route("/api/engines/{id}/install", post(engine_install))
        // Web-native, like /api/transcript: the Electron bridge had no recast
        // channel, and adding one to the generated enum would make the parity
        // numbers describe a contract that never existed.
        .route("/api/recast", post(recast_route))
        // Account management. These name `Admin` in their signatures, so the
        // check is in the type rather than in a middleware someone has to
        // remember to attach.
        .route("/api/accounts", get(accounts_list).post(accounts_create))
        .route("/api/accounts/{user}", post(accounts_update))
        // Web-native, so it lives under /api rather than /rpc: the Electron
        // bridge had no transcript channel (it showed a terminal), and adding
        // one to the generated enum would make the parity numbers describe a
        // contract that never existed.
        .route("/api/transcript", get(transcript_route))
        // Inbound webhooks. Public and secret-gated, so deliberately OUTSIDE
        // the session-authenticated surface — and on this same server, which is
        // why the Electron version's second HTTP server and tunnel disappear.
        .route("/hooks/{tenant}/{id}", post(webhook_post).get(webhook_status_get))
        .route("/rpc/{channel}", post(rpc::rpc_handler))
        .route("/ws", get(ws::ws_handler));

    // Slack posts events to the same inbound surface as any other webhook; an
    // endpoint with id `slack` receives them, so there is no second listener.
    // The message is announced so the floor can show it arriving.
    let _ = &state;

    // The built WASM client, when present. Absent in tests and in API-only runs.
    if let Some(dir) = &cfg.static_dir {
        app = app.fallback_service(tower_http::services::ServeDir::new(dir));
    }

    Ok(app.with_state(state))
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "portedChannels": md_contract::Rpc::ALL.len() - rpc::unported().len() - rpc::not_applicable().len(),
        "totalChannels": md_contract::Rpc::ALL.len(),
        // Split out so "what is left" is real work, not padded with channels
        // that will never be ported.
        "todoChannels": rpc::unported().len(),
        "notApplicableChannels": rpc::not_applicable().len(),
    }))
}

#[derive(serde::Deserialize)]
pub struct LoginBody { pub user: String, pub password: String }

async fn login(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(body): Json<LoginBody>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    // Same response whether the user is unknown, disabled, or the password is
    // wrong, so the endpoint does not enumerate accounts.
    let ok = state.accounts.get(&body.user).filter(|a| a.verify(&body.password));
    let Some(account) = ok else {
        return (axum::http::StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "invalid credentials" }))).into_response();
    };

    let Ok(tenant) = TenantId::parse(&account.tenant) else {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "account has an invalid tenant" }))).into_response();
    };
    let token = state.sessions.create(tenant.clone(), account.user.clone(), account.role.is_admin());
    // HttpOnly keeps the token away from page scripts; SameSite=Strict is the
    // CSRF control for the cookie path.
    let cookie = format!(
        "md_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=604800"
    );
    (
        axum::http::StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        // Also returned in the body for non-browser clients and the WS handshake.
        // The role travels back so the client can show the admin panel without
        // a second call — it is not a capability, only a hint about the UI.
        Json(serde_json::json!({
            "token": token, "tenant": tenant.as_str(),
            "user": account.user, "role": account.role,
        })),
    ).into_response()
}

/// Answer every rejection identically.
///
/// An unknown tenant, an unknown endpoint and a wrong secret must be
/// indistinguishable — otherwise the surface can be walked to discover which
/// tenants and endpoints exist.
fn webhook_denied() -> (axum::http::StatusCode, Json<serde_json::Value>) {
    (
        axum::http::StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "ok": false, "error": "unauthorized" })),
    )
}

/// Turn an external POST into hive work.
async fn webhook_post(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path((tenant, id)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    body: String,
) -> impl axum::response::IntoResponse {
    // Rate limit FIRST, before any parsing or comparison: the point is to bound
    // the work an unauthenticated caller can cause. Rejected requests consume
    // the budget too, or guessing secrets would be free.
    if !webhooks::allow(&format!("{tenant}/{id}")) {
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "ok": false, "error": "rate limited" })),
        );
    }
    if body.len() > webhooks::MAX_BODY {
        return (
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({ "ok": false, "error": "body too large" })),
        );
    }

    let presented = headers
        .get("x-md-webhook-secret")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let Ok(tid) = TenantId::parse(&tenant) else { return webhook_denied() };
    let Some(paths) = state.paths(&tid) else { return webhook_denied() };
    let Some(endpoint) = webhooks::Webhooks::new(&paths.harness_home()).authenticate(&id, presented)
    else {
        return webhook_denied();
    };

    // The token is minted here and returned ONCE. Only its hash is persisted, so
    // a leak of the task ledger does not leak the tokens.
    let token = webhooks::mint_token();
    let payload: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|_| serde_json::json!({ "raw": body }));

    let task_id = format!("wh-{}", webhooks::random_hex(8));
    let hive = hive::Hive::new(paths.hive_root());
    hive.add_task(serde_json::json!({
        "id": task_id,
        "title": payload.get("title").and_then(|v| v.as_str())
            .unwrap_or_else(|| endpoint["name"].as_str().unwrap_or("Webhook request")),
        "description": payload.get("body").and_then(|v| v.as_str()).unwrap_or(&body),
        "status": "todo",
        "priority": 2,
        "dependsOn": [],
        "createdAt": hive::iso_now(),
        // The HASH, never the token.
        "webhook": { "tokenHash": webhooks::hash_token(&token) },
    }));

    // The endpoint's public fields only — the record never reaches the message.
    let sent = hive.send(
        &serde_json::json!({
            "to": "god",
            "act": "request",
            "subject": format!("Webhook: {}", endpoint["name"].as_str().unwrap_or(&id)),
            "body": format!("An external caller triggered `{id}`. Card `{task_id}` is on the board.\n\n{payload}"),
        }),
        "webhook",
    );
    let delivered: Vec<String> = sent["delivered"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    handlers::announce_routed(&state, &tid, &paths, &sent["message"], &delivered);

    // An endpoint named `slack` is Slack's inbound channel. Announced on its own
    // push so the floor can distinguish a chat message from a generic trigger.
    if id == "slack" {
        state.hub.publish(
            &tid,
            md_contract::ServerEvent::new(
                md_contract::Push::SlackIncomingMessage,
                serde_json::json!({
                    "text": payload.get("text"),
                    "channel": payload.get("channel"),
                    "thread_ts": payload.get("thread_ts"),
                }),
            ),
        );
    }

    (
        axum::http::StatusCode::OK,
        Json(serde_json::json!({ "ok": true, "token": token, "taskId": task_id })),
    )
}

/// Report the status of the ONE task a token maps to.
///
/// The lookup runs whether or not the tenant or endpoint exists, and the
/// not-found answer is identical in every case — same reason as the POST gate.
async fn webhook_status_get(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path((tenant, id)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl axum::response::IntoResponse {
    if !webhooks::allow(&format!("{tenant}/{id}")) {
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "ok": false })),
        );
    }
    let token = headers
        .get("x-md-webhook-token")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
        .or_else(|| q.get("token").cloned())
        .unwrap_or_default();
    let want = webhooks::hash_token(&token);

    let not_found = (
        axum::http::StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "ok": false, "error": "not found" })),
    );
    let Ok(tid) = TenantId::parse(&tenant) else { return not_found };
    let Some(paths) = state.paths(&tid) else { return not_found };

    let tasks = hive::Hive::new(paths.hive_root()).tasks();
    let Some(card) = tasks["tasks"].as_array().and_then(|list| {
        list.iter()
            .find(|t| !token.is_empty() && t.pointer("/webhook/tokenHash").and_then(|v| v.as_str()) == Some(&want))
    }) else {
        return not_found;
    };

    // Only this card, and only these fields: a capability token is not a
    // licence to read the board.
    (
        axum::http::StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "status": card["status"],
            "title": card["title"],
            "result": card.get("result"),
        })),
    )
}

#[derive(serde::Deserialize)]
pub struct TranscriptQuery {
    /// Which agent's session to read.
    pub agent: String,
    /// Byte offset from the previous page. Absent means start at the beginning.
    #[serde(default)]
    pub cursor: u64,
}

/// The conversation view's source.
///
/// Resolves the agent's session through the registry — `cwd` plus the
/// `sessionId` the hook server recorded — so the client never handles a
/// filesystem path.
async fn transcript_route(
    axum::extract::State(state): axum::extract::State<AppState>,
    auth::Tenant(tenant): auth::Tenant,
    axum::extract::Query(q): axum::extract::Query<TranscriptQuery>,
) -> Json<serde_json::Value> {
    let Some(paths) = state.paths(&tenant) else {
        return Json(serde_json::json!({ "error": "unknown tenant" }));
    };
    let reg = hive::Hive::new(paths.hive_root()).registry();
    let agent = &reg["agents"][&q.agent];
    let (Some(cwd), Some(session)) = (
        agent["cwd"].as_str(),
        agent["sessionId"].as_str().filter(|s| !s.is_empty()),
    ) else {
        // Not an error: an agent that has not reported a session yet simply has
        // nothing to show, and the view polls until it does.
        return Json(serde_json::json!({ "entries": [], "cursor": 0, "more": false, "waiting": true }));
    };

    let Some(file) = transcript::session_file(&paths.home(), cwd, session) else {
        return Json(serde_json::json!({ "entries": [], "cursor": 0, "more": false, "waiting": true }));
    };
    match transcript::read(&file, q.cursor) {
        Ok(page) => Json(serde_json::to_value(page).unwrap_or_default()),
        Err(e) => Json(serde_json::json!({ "error": e.to_string() })),
    }
}

/// Who am I, and may I manage accounts? Lets a reloaded client restore its own
/// state without a second sign-in.
async fn me(auth::Auth(session): auth::Auth) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "user": session.user, "tenant": session.tenant.as_str(), "admin": session.admin,
    }))
}

/// The MCP catalog, with this tenant's consent applied.
async fn mcp_catalog(
    axum::extract::State(state): axum::extract::State<AppState>,
    auth::Tenant(tenant): auth::Tenant,
) -> Json<serde_json::Value> {
    let cfg = state
        .paths(&tenant)
        .and_then(|p| std::fs::read_to_string(p.config_file()).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    Json(serde_json::json!({ "catalog": mcp::catalog_view(&cfg) }))
}

/// The engines this tenant can hire, with the built-ins its config has changed
/// already merged in, and whether each one's command is actually installed.
async fn engine_catalog(
    axum::extract::State(state): axum::extract::State<AppState>,
    auth::Tenant(tenant): auth::Tenant,
) -> Json<serde_json::Value> {
    let cfg = state
        .paths(&tenant)
        .and_then(|p| std::fs::read_to_string(p.config_file()).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    Json(serde_json::json!({ "engines": engines::view(&cfg) }))
}

/// How long an install is given before it is abandoned.
///
/// Generous, because a cold npm install of a large CLI on a slow link is
/// genuinely minutes — and a timeout that fires early leaves a half-installed
/// package with nothing to say about it.
const INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Install an engine's command.
///
/// **Admin only, and operators only.** This runs a shell command on the server
/// host, which is a strictly larger privilege than anything else here: an agent
/// may PROPOSE a tool and never arm it, and it may not reach this at all. The
/// command run is the one in the catalogue or the one this tenant wrote into its
/// own config — in both cases something a person put there.
async fn engine_install(
    axum::extract::State(state): axum::extract::State<AppState>,
    auth::Admin(session): auth::Admin,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let cfg = state
        .paths(&session.tenant)
        .and_then(|p| std::fs::read_to_string(p.config_file()).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    let Some(engine) = engines::resolve(&cfg, &id) else {
        return Json(serde_json::json!({ "ok": false, "error": format!("no engine `{id}`") }));
    };
    if engine.install.trim().is_empty() {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!(
                "there is no known way to install {}. Set one under \
                 engines.{id}.install in this floor's config and try again.",
                engine.label
            ),
        }));
    }

    // The agent PATH, so the install lands where agents will look for it — and
    // so a command that itself needs node finds the node the agents use.
    let run = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&engine.install)
        .env("PATH", md_pty::env::agent_path())
        .output();

    let out = match tokio::time::timeout(INSTALL_TIMEOUT, run).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Json(serde_json::json!({ "ok": false, "error": format!("could not run it: {e}") }))
        }
        Err(_) => {
            return Json(serde_json::json!({
                "ok": false,
                "error": "the install ran for ten minutes without finishing and was abandoned",
            }))
        }
    };

    // The tail of both streams: npm's useful part is at the end, and the whole
    // of it is not something to put through a status line.
    let tail = |b: &[u8]| {
        let t = String::from_utf8_lossy(b);
        t.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev()
            .collect::<Vec<_>>().join("\n")
    };
    let ok = out.status.success();
    Json(serde_json::json!({
        "ok": ok,
        // Re-checked rather than assumed: an installer can exit 0 and still not
        // put the command anywhere the agents will find it, which is the whole
        // failure this panel exists to make visible.
        "available": engines::available(&engine.command),
        "output": if ok { tail(&out.stdout) } else { tail(&out.stderr) },
        "error": if ok { serde_json::Value::Null } else {
            serde_json::json!(format!("install failed: {}", out.status))
        },
    }))
}

/// Rename the floor to a theme's cast, rebuilding each agent's identity.
async fn recast_route(
    axum::extract::State(state): axum::extract::State<AppState>,
    auth::Tenant(tenant): auth::Tenant,
    Json(cast): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let Some(paths) = state.paths(&tenant) else {
        return Json(serde_json::json!({ "ok": false, "error": "unknown tenant" }));
    };
    let ctx = handlers::Ctx { state, tenant, paths, args: vec![cast] };
    match handlers::recast(&ctx) {
        md_contract::RpcResponse::Ok { result } => Json(result),
        md_contract::RpcResponse::Err { error } => {
            Json(serde_json::json!({ "ok": false, "error": error.message }))
        }
    }
}

async fn accounts_list(
    axum::extract::State(state): axum::extract::State<AppState>,
    auth::Admin(_): auth::Admin,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "accounts": state.accounts.list() }))
}

#[derive(serde::Deserialize)]
pub struct NewAccount {
    pub user: String,
    pub tenant: String,
    pub password: String,
    #[serde(default = "member")]
    pub role: accounts::Role,
}

fn member() -> accounts::Role {
    accounts::Role::Member
}

async fn accounts_create(
    axum::extract::State(state): axum::extract::State<AppState>,
    auth::Admin(_): auth::Admin,
    Json(body): Json<NewAccount>,
) -> Json<serde_json::Value> {
    // Refuse a new tenant the sandbox cannot isolate, HERE — at the point of the
    // mistake. Without this the account is created, the server refuses to start
    // on its next boot, and the operator learns about it from a crash loop with
    // no obvious connection to what they did.
    let known = state.paths(&TenantId::parse(&body.tenant).unwrap_or_else(|_| {
        TenantId::parse("invalid").expect("a literal that always parses")
    }));
    if known.is_none() {
        let count = state.tenant_ids().len() + 1;
        if let Err(e) = state.sandbox.preflight(count) {
            return Json(serde_json::json!({
                "ok": false,
                "error": format!("cannot add the tenant '{}': {e}", body.tenant),
            }));
        }
    }

    match state.accounts.create(&body.user, &body.tenant, body.role, &body.password) {
        // A new tenant needs its directories before its owner can do anything.
        Ok(()) => {
            // Register the tenant NOW, not at the next restart: an account that
            // authenticates into a tenant the server does not know about reads
            // as a broken server rather than a missing step.
            if let Ok(t) = TenantId::parse(&body.tenant) {
                let data_root = state.data_root.clone();
                state.ensure_tenant(&t, &data_root);
                spawn_tenant_services(&state, &t);
            }
            Json(serde_json::json!({ "ok": true }))
        }
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e })),
    }
}

#[derive(serde::Deserialize)]
pub struct AccountPatch {
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub role: Option<accounts::Role>,
    #[serde(default)]
    pub disabled: Option<bool>,
}

/// Change one account.
///
/// A password change is the one operation a NON-admin may perform, and only on
/// themselves — so this takes `Auth` and checks the role per field rather than
/// demanding `Admin` for the whole route.
async fn accounts_update(
    axum::extract::State(state): axum::extract::State<AppState>,
    auth::Auth(session): auth::Auth,
    axum::extract::Path(user): axum::extract::Path<String>,
    Json(patch): Json<AccountPatch>,
) -> Json<serde_json::Value> {
    let Some(actor) = state.accounts.get(&session.user) else {
        return Json(serde_json::json!({ "ok": false, "error": "unknown actor" }));
    };

    if let Some(p) = &patch.password {
        if let Err(e) = state.accounts.set_password(&actor, &user, p) {
            return Json(serde_json::json!({ "ok": false, "error": e }));
        }
    }
    // Role and disabled are admin-only, whoever the target is: changing your own
    // role is how a member would promote themselves.
    if patch.role.is_some() || patch.disabled.is_some() {
        if !actor.role.is_admin() {
            return Json(serde_json::json!({ "ok": false, "error": "this action requires an admin account" }));
        }
        if let Some(r) = patch.role {
            if let Err(e) = state.accounts.set_role(&user, r) {
                return Json(serde_json::json!({ "ok": false, "error": e }));
            }
        }
        if let Some(d) = patch.disabled {
            if let Err(e) = state.accounts.set_disabled(&user, d) {
                return Json(serde_json::json!({ "ok": false, "error": e }));
            }
        }
    }
    Json(serde_json::json!({ "ok": true }))
}

async fn logout(
    axum::extract::State(state): axum::extract::State<AppState>,
    auth::Auth(_session): auth::Auth,
    headers: axum::http::HeaderMap,
) -> Json<serde_json::Value> {
    if let Some(tok) = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| c.split(';').find_map(|kv| kv.trim().strip_prefix("md_session=")))
    {
        state.sessions.revoke(tok);
    }
    Json(serde_json::json!({ "ok": true }))
}
