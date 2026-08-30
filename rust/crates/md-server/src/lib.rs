//! Munder Difflin server: the Rust replacement for the Electron main process.

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

use auth::{Account, SessionStore};
use state::{AppState, ServerConfig};
use ws::Hub;

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
pub async fn build(cfg: &ServerConfig, accounts: Vec<Account>, sandbox: Arc<dyn Sandbox>)
    -> anyhow::Result<Router>
{
    let tenants: HashMap<TenantId, TenantPaths> = accounts
        .iter()
        .map(|a| (a.tenant.clone(), TenantPaths::new(&cfg.data_root, a.tenant.clone())))
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
        accounts: Arc::new(accounts.into_iter().map(|a| (a.user.clone(), a)).collect()),
        tenants: Arc::new(tenants),
        pty: Arc::new(PtyManager::new(Arc::clone(&sandbox))),
        hub: Hub::new(),
        sandbox,
        control: Arc::new(control),
        closing: Arc::new(closing),
        realtime: Arc::new(realtime),
    };

    // One hook listener per tenant, inside that tenant's own hive directory.
    // The socket path is the authorization, so there is nothing further to check
    // on the connection itself.
    for (tenant, paths) in state.tenants.iter() {
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
    }

    // The outbox router, one task per tenant. Polling rather than filesystem
    // watching: agents write these files by hand from arbitrary processes, and a
    // poll is cheap and does not depend on platform watch semantics.
    for (tenant, paths) in state.tenants.iter() {
        let (state, tenant, paths) = (state.clone(), tenant.clone(), paths.clone());
        tokio::spawn(async move {
            let hive = hive::Hive::new(paths.hive_root());
            let mut tick = tokio::time::interval(ROUTER_INTERVAL);
            loop {
                tick.tick().await;
                // Blocking file IO, so it does not belong on the async worker.
                let hive = hive.clone();
                let Ok(routed) = tokio::task::spawn_blocking(move || hive.route_once()).await
                else {
                    continue;
                };
                for r in routed {
                    handlers::announce_routed(&state, &tenant, &paths, &r.message, &r.delivered);
                }
            }
        });
    }

    let mut app = Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/health", get(health))
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

    // Same response whether the user is unknown or the password is wrong, so the
    // endpoint does not enumerate accounts.
    let ok = state.accounts.get(&body.user).filter(|a| a.verify(&body.password));
    let Some(account) = ok else {
        return (axum::http::StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "invalid credentials" }))).into_response();
    };

    let token = state.sessions.create(account.tenant.clone(), account.user.clone());
    // HttpOnly keeps the token away from page scripts; SameSite=Strict is the
    // CSRF control for the cookie path.
    let cookie = format!(
        "md_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=604800"
    );
    (
        axum::http::StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        // Also returned in the body for non-browser clients and the WS handshake.
        Json(serde_json::json!({ "token": token, "tenant": account.tenant.as_str() })),
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
    handlers::announce_routed(&state, &tid, paths, &sent["message"], &delivered);

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
