//! Authentication and the tenant extractor.
//!
//! Authorization is an extractor, not a middleware check, for a specific reason:
//! with 158 RPC channels, a middleware that has to be *remembered* on each route
//! will eventually be forgotten on one. A handler that needs tenant scope has to
//! name [`Tenant`] in its signature to get it, so an unauthorized handler is one
//! that cannot compile against tenant-scoped state.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::FromRequestParts;
use axum::http::{header, request::Parts, StatusCode};
use md_tenant::TenantId;

/// An authenticated browser session.
#[derive(Debug, Clone)]
pub struct Session {
    pub tenant: TenantId,
    pub user: String,
}

#[derive(Clone, Default)]
pub struct SessionStore {
    inner: Arc<RwLock<HashMap<String, Session>>>,
}

impl SessionStore {
    pub fn new() -> Self { Self::default() }

    pub fn create(&self, tenant: TenantId, user: String) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        self.inner.write().unwrap().insert(token.clone(), Session { tenant, user });
        token
    }

    pub fn get(&self, token: &str) -> Option<Session> {
        self.inner.read().unwrap().get(token).cloned()
    }

    pub fn revoke(&self, token: &str) { self.inner.write().unwrap().remove(token); }
}

/// A stored account. Passwords are Argon2id; the hash string carries its own
/// salt and parameters so no separate columns are needed.
#[derive(Clone)]
pub struct Account {
    pub user: String,
    pub tenant: TenantId,
    pub password_hash: String,
}

impl Account {
    pub fn new(user: &str, tenant: TenantId, password: &str) -> anyhow::Result<Self> {
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("hashing failed: {e}"))?
            .to_string();
        Ok(Self { user: user.to_string(), tenant, password_hash: hash })
    }

    pub fn verify(&self, password: &str) -> bool {
        let Ok(parsed) = PasswordHash::new(&self.password_hash) else { return false };
        Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_verification_accepts_only_the_right_password() {
        let t = TenantId::parse("acme").unwrap();
        let a = Account::new("dae", t, "correct horse").unwrap();
        assert!(a.verify("correct horse"));
        assert!(!a.verify("Correct horse"));
        assert!(!a.verify(""));
    }

    /// Two accounts with the same password must not share a hash, or the store
    /// leaks which users chose the same password.
    #[test]
    fn hashes_are_salted_per_account() {
        let t = TenantId::parse("acme").unwrap();
        let a = Account::new("a", t.clone(), "same").unwrap();
        let b = Account::new("b", t, "same").unwrap();
        assert_ne!(a.password_hash, b.password_hash);
    }

    #[test]
    fn revoked_sessions_stop_resolving() {
        let store = SessionStore::new();
        let tok = store.create(TenantId::parse("acme").unwrap(), "dae".into());
        assert!(store.get(&tok).is_some());
        store.revoke(&tok);
        assert!(store.get(&tok).is_none());
    }
}
