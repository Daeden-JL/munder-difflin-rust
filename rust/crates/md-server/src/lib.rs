//! Munder Difflin server: the Rust replacement for the Electron main process.

pub mod auth;
pub mod closing;
pub mod control;
pub mod git;
pub mod hooks;
pub mod hive;
pub mod handlers;
pub mod rpc;
pub mod spawn;
pub mod state;
pub mod transcript;
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

    let state = AppState {
        sessions: SessionStore::new(),
        accounts: Arc::new(accounts.into_iter().map(|a| (a.user.clone(), a)).collect()),
        tenants: Arc::new(tenants),
        pty: Arc::new(PtyManager::new(Arc::clone(&sandbox))),
        hub: Hub::new(),
        sandbox,
        control: Arc::new(control),
        closing: Arc::new(closing),
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
        .route("/rpc/{channel}", post(rpc::rpc_handler))
        .route("/ws", get(ws::ws_handler));

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
