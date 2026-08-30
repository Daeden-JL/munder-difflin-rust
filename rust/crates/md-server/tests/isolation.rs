//! Isolation properties that must not regress.
//!
//! These are the tests that matter most in a multi-tenant build: each one
//! encodes a boundary that, if it breaks, leaks one tenant's work to another.

use std::sync::Arc;

use md_server::auth::Account;
use md_server::state::ServerConfig;
use md_tenant::sandbox::PassthroughSandbox;
use md_tenant::{Sandbox, TenantId};

fn cfg(dir: &std::path::Path) -> ServerConfig {
    ServerConfig {
        bind: "127.0.0.1:0".into(),
        data_root: dir.to_path_buf(),
        static_dir: None,
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("md-test-{name}-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// The single most important startup check: passthrough gives no isolation, so
/// booting it with more than one tenant would silently expose every tenant to
/// every other. It must be a hard startup failure, not a warning.
#[tokio::test]
async fn passthrough_refuses_to_serve_two_tenants() {
    let dir = tmp("passthrough-multi");
    let accounts = vec![
        Account::new("a", TenantId::parse("alpha").unwrap(), "pw").unwrap(),
        Account::new("b", TenantId::parse("beta").unwrap(), "pw").unwrap(),
    ];
    let err = md_server::build(&cfg(&dir), accounts, Arc::new(PassthroughSandbox)).await
        .expect_err("must refuse to start");
    assert!(err.to_string().contains("no isolation"), "unexpected error: {err}");
}

#[tokio::test]
async fn passthrough_serves_a_single_tenant() {
    let dir = tmp("passthrough-single");
    let accounts = vec![Account::new("a", TenantId::parse("alpha").unwrap(), "pw").unwrap()];
    assert!(md_server::build(&cfg(&dir), accounts, Arc::new(PassthroughSandbox)).await.is_ok());
}

/// Provisioning must create each tenant's home eagerly. A tenant whose directory
/// appears lazily on first write is a tenant whose first read silently falls
/// through to whatever the parent directory happens to contain.
#[tokio::test]
async fn building_provisions_each_tenant_home() {
    let dir = tmp("provision");
    let accounts = vec![Account::new("a", TenantId::parse("gamma").unwrap(), "pw").unwrap()];
    let _app = md_server::build(&cfg(&dir), accounts, Arc::new(PassthroughSandbox)).await.unwrap();
    assert!(dir.join("gamma/.munder-difflin").is_dir());
    assert!(dir.join("gamma/workspaces").is_dir());
}

/// A sandbox must never resolve one tenant's spawn against another's paths, even
/// when both are legitimate tenants on the same server.
#[tokio::test]
async fn sandbox_refuses_cross_tenant_spawn() {
    use md_tenant::{SpawnRequest, TenantPaths};
    let alpha = TenantId::parse("alpha").unwrap();
    let beta = TenantId::parse("beta").unwrap();
    let req = SpawnRequest {
        tenant: alpha,
        program: "sh".into(),
        args: vec![],
        cwd: "/srv/md/beta/workspaces/p".into(),
        env: Default::default(),
        cols: 80,
        rows: 24,
    };
    let beta_paths = TenantPaths::new("/srv/md", beta);
    assert!(PassthroughSandbox.resolve(&req, &beta_paths).is_err());
}

/// The fs channels take `(root, rel)` — BOTH, not one joined path.
///
/// This is a parity bug that shipped once: the handlers read only argument 0,
/// so `readFile(root, "policy.md")` tried to read the directory. It passed
/// every unit test, because the unit tests called it the wrong way too. The
/// check that matters is against the bridge's own signature.
#[test]
fn the_fs_channels_take_a_root_and_a_relative_path() {
    let manifest: serde_json::Value =
        serde_json::from_str(include_str!("../../../../contract/manifest.json")).unwrap();

    let params = |name: &str| -> Vec<String> {
        manifest["methods"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .and_then(|m| m["params"].as_array())
            .map(|ps| ps.iter().filter_map(|p| p["name"].as_str().map(String::from)).collect())
            .unwrap_or_default()
    };

    assert_eq!(params("readFile"), ["root", "rel"]);
    assert_eq!(params("writeFile"), ["root", "rel", "content"]);
    assert_eq!(params("listDir"), ["root", "rel"]);
}
