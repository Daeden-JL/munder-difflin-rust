//! Shared server state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::FromRef;
use md_pty::PtyManager;
use md_tenant::{Sandbox, TenantId, TenantPaths};

use crate::auth::{Account, SessionStore};
use crate::closing::Closing;
use crate::realtime::Realtime;
use crate::control::Control;
use crate::ws::Hub;

#[derive(Clone)]
pub struct AppState {
    pub sessions: SessionStore,
    pub accounts: Arc<HashMap<String, Account>>,
    pub tenants: Arc<HashMap<TenantId, TenantPaths>>,
    pub pty: Arc<PtyManager>,
    pub hub: Hub,
    pub sandbox: Arc<dyn Sandbox>,
    /// Operator control state, per tenant. In memory because it describes agents
    /// that are running now — it is meaningless across a restart that took them.
    pub control: Arc<HashMap<TenantId, Arc<Control>>>,
    /// Closing-time state, per tenant. Winding down one tenant's floor must
    /// never touch another's.
    pub closing: Arc<HashMap<TenantId, Arc<Closing>>>,
    /// Voice-session state, per tenant: proposals awaiting confirmation and
    /// completions waiting to be drained.
    pub realtime: Arc<HashMap<TenantId, Arc<Realtime>>>,
}

impl AppState {
    pub fn paths(&self, tenant: &TenantId) -> Option<&TenantPaths> {
        self.tenants.get(tenant)
    }

    /// Every provisioned tenant has a registry, so this cannot legitimately miss.
    pub fn control(&self, tenant: &TenantId) -> Arc<Control> {
        self.control.get(tenant).cloned().unwrap_or_default()
    }

    pub fn closing(&self, tenant: &TenantId) -> Arc<Closing> {
        self.closing.get(tenant).cloned().unwrap_or_default()
    }

    pub fn realtime(&self, tenant: &TenantId) -> Arc<Realtime> {
        self.realtime.get(tenant).cloned().unwrap_or_default()
    }
}

impl FromRef<AppState> for SessionStore {
    fn from_ref(s: &AppState) -> Self { s.sessions.clone() }
}

impl FromRef<AppState> for Hub {
    fn from_ref(s: &AppState) -> Self { s.hub.clone() }
}

/// Server configuration, resolved at startup.
pub struct ServerConfig {
    pub bind: String,
    /// Parent directory holding every tenant home.
    pub data_root: PathBuf,
    pub static_dir: Option<PathBuf>,
}
