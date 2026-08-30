//! Shared server state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use axum::extract::FromRef;
use md_pty::PtyManager;
use md_tenant::{Sandbox, TenantId, TenantPaths};

use crate::accounts::Accounts;
use crate::auth::SessionStore;
use crate::closing::Closing;
use crate::realtime::Realtime;
use crate::control::Control;
use crate::ws::Hub;

#[derive(Clone)]
pub struct AppState {
    pub sessions: SessionStore,
    /// The persisted account store. Was a HashMap seeded from the environment,
    /// which could not be changed without a restart or revoked at all.
    pub accounts: Arc<Accounts>,
    /// Needed to provision a tenant when an admin creates its first account.
    pub data_root: PathBuf,
    /// Tenants, and where their data lives.
    ///
    /// Behind a lock because an admin can create an account in a NEW tenant
    /// while the server runs — with a frozen map that account would
    /// authenticate and then find no home, which reads as a broken server
    /// rather than a missing step.
    pub tenants: Arc<RwLock<HashMap<TenantId, TenantPaths>>>,
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
    pub fn paths(&self, tenant: &TenantId) -> Option<TenantPaths> {
        self.tenants.read().unwrap().get(tenant).cloned()
    }

    /// Register a tenant discovered after startup, provisioning its
    /// directories. Idempotent: re-registering an existing tenant is a no-op.
    pub fn ensure_tenant(&self, tenant: &TenantId, data_root: &std::path::Path) -> TenantPaths {
        if let Some(p) = self.paths(tenant) {
            return p;
        }
        let paths = TenantPaths::new(data_root, tenant.clone());
        let _ = std::fs::create_dir_all(paths.harness_home());
        let _ = std::fs::create_dir_all(paths.workspaces());
        self.tenants.write().unwrap().insert(tenant.clone(), paths.clone());
        tracing::info!(%tenant, "registered a tenant created at runtime");
        paths
    }

    pub fn tenant_ids(&self) -> Vec<TenantId> {
        self.tenants.read().unwrap().keys().cloned().collect()
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
