//! Authentication and the tenant extractor.
//!
//! Authorization is an extractor, not a middleware check, for a specific reason:
//! with 158 RPC channels, a middleware that has to be *remembered* on each route
//! will eventually be forgotten on one. A handler that needs tenant scope has to
//! name [`Tenant`] in its signature to get it, so an unauthorized handler is one
//! that cannot compile against tenant-scoped state.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::extract::FromRequestParts;
use axum::http::{header, request::Parts, StatusCode};
use md_tenant::TenantId;

/// An authenticated browser session.
#[derive(Debug, Clone)]
pub struct Session {
    pub tenant: TenantId,
    pub user: String,
    /// Carried on the session so an admin route can demand it in its signature,
    /// the same way a tenant route demands `Tenant` — a check you must name to
    /// get is a check you cannot forget.
    pub admin: bool,
}

#[derive(Clone, Default)]
pub struct SessionStore {
    inner: Arc<RwLock<HashMap<String, Session>>>,
}

impl SessionStore {
    pub fn new() -> Self { Self::default() }

    pub fn create(&self, tenant: TenantId, user: String, admin: bool) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        self.inner.write().unwrap().insert(token.clone(), Session { tenant, user, admin });
        token
    }

    pub fn get(&self, token: &str) -> Option<Session> {
        self.inner.read().unwrap().get(token).cloned()
    }

    pub fn revoke(&self, token: &str) { self.inner.write().unwrap().remove(token); }
}


/// Extractor yielding the caller's tenant. Rejects with 401 when absent.
#[derive(Debug, Clone)]
pub struct Tenant(pub TenantId);

/// Extractor yielding the whole session when the handler needs the user too.
#[derive(Debug, Clone)]
pub struct Auth(pub Session);

impl<S> FromRequestParts<S> for Auth
where
    SessionStore: axum::extract::FromRef<S>,
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        use axum::extract::FromRef;
        let store = SessionStore::from_ref(state);

        // Bearer first (programmatic clients and the WS handshake), then cookie
        // (the browser app). Both name the same session.
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer ").map(str::to_string))
            .or_else(|| {
                parts.headers.get(header::COOKIE).and_then(|v| v.to_str().ok()).and_then(|c| {
                    c.split(';').find_map(|kv| {
                        let kv = kv.trim();
                        kv.strip_prefix("md_session=").map(str::to_string)
                    })
                })
            })
            .ok_or((StatusCode::UNAUTHORIZED, "missing session"))?;

        let session = store.get(&token).ok_or((StatusCode::UNAUTHORIZED, "invalid session"))?;
        Ok(Auth(session))
    }
}

impl<S> FromRequestParts<S> for Tenant
where
    SessionStore: axum::extract::FromRef<S>,
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Auth(session) = Auth::from_request_parts(parts, state).await?;
        Ok(Tenant(session.tenant))
    }
}

/// An authenticated ADMIN session.
///
/// A route that manages accounts names this instead of [`Auth`]. The check is
/// therefore in the signature, not in a middleware someone has to remember to
/// attach to the right routes.
pub struct Admin(pub Session);

impl<S> FromRequestParts<S> for Admin
where
    SessionStore: axum::extract::FromRef<S>,
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Auth(session) = Auth::from_request_parts(parts, state).await?;
        if !session.admin {
            // 403, not 404: the caller IS authenticated, and pretending the
            // route does not exist would be a puzzle rather than an answer.
            return Err((StatusCode::FORBIDDEN, "this action requires an admin account"));
        }
        Ok(Admin(session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Password hashing and salting are tested in `accounts`, which owns the
    // account type. What belongs here is session lifetime.

    #[test]
    fn revoked_sessions_stop_resolving() {
        let store = SessionStore::new();
        let tok = store.create(TenantId::parse("acme").unwrap(), "dae".into(), false);
        assert!(store.get(&tok).is_some());
        store.revoke(&tok);
        assert!(store.get(&tok).is_none());
    }
}

