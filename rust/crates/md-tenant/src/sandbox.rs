//! The execution boundary for spawned agents.
//!
//! Every agent process is created through a [`Sandbox`]. The trait exists so the
//! isolation mechanism is a deployment choice rather than something baked into
//! the PTY layer, and so the PTY layer physically cannot spawn an unconfined
//! child: it holds a `&dyn Sandbox`, not a `Command`.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::{TenantId, TenantPaths};

/// Which isolation mechanism a deployment uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxKind {
    /// No isolation: the child runs as the server user. Reproduces today's
    /// desktop behavior and is the only mode that may be used with a single
    /// trusted tenant on a machine that tenant already controls.
    ///
    /// Refuses to start when more than one tenant is configured — see
    /// [`Sandbox::preflight`]. That check is the difference between "a dev
    /// convenience" and "a multi-tenant breach".
    Passthrough,
    /// One unix account per tenant; children are spawned with that uid/gid.
    /// Isolation is ordinary filesystem permissions, which is well understood,
    /// but shared kernel and shared `/tmp` mean it is weaker than a container.
    LocalUid,
    /// One container per tenant. The deployable default: filesystem, process
    /// table, and network namespace are all separated.
    Container,
}

/// A request to run one agent process. Deliberately not a `std::process::Command`:
/// the sandbox decides how the request becomes a process, and the caller must not
/// be able to smuggle in a pre-built command that bypasses that decision.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub tenant: TenantId,
    /// Program name as the user configured it (`claude`, `codex`, …). Resolution
    /// against PATH happens inside the sandbox, because PATH differs per backend.
    pub program: String,
    pub args: Vec<String>,
    /// Working directory, already validated to sit inside the tenant home.
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("sandbox refused the request: {0}")]
    Refused(String),
    #[error("path {path} is outside tenant {tenant}'s home")]
    OutsideTenantHome { tenant: TenantId, path: PathBuf },
    #[error("sandbox backend {0:?} is not available on this host: {1}")]
    Unavailable(SandboxKind, String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// How a backend turns a validated request into something spawnable.
///
/// Returns the argv to actually execute rather than spawning directly, so the PTY
/// layer keeps ownership of the pty pair and this crate stays free of pty
/// dependencies. The wrapping is the isolation: `LocalUid` prefixes a
/// privilege-dropping exec, `Container` prefixes the container runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
}

pub trait Sandbox: Send + Sync {
    fn kind(&self) -> SandboxKind;

    /// Fail fast at startup rather than at first spawn.
    ///
    /// A misconfigured sandbox that only reveals itself when an agent starts is
    /// the worst case: by then a tenant is watching a terminal that will never
    /// produce output, and on `Passthrough` the failure mode is silent
    /// cross-tenant access rather than an error.
    fn preflight(&self, tenant_count: usize) -> Result<(), SandboxError>;

    fn resolve(&self, req: &SpawnRequest, paths: &TenantPaths)
        -> Result<ResolvedCommand, SandboxError>;
}

/// Shared validation every backend must apply before its own wrapping.
///
/// Backends call this rather than reimplementing it, so a new backend cannot ship
/// without the containment check.
fn validate(req: &SpawnRequest, paths: &TenantPaths) -> Result<(), SandboxError> {
    if paths.id() != &req.tenant {
        return Err(SandboxError::Refused(format!(
            "request for tenant {} routed to paths for {}", req.tenant, paths.id()
        )));
    }
    if !paths.contains(&req.cwd) {
        return Err(SandboxError::OutsideTenantHome {
            tenant: req.tenant.clone(),
            path: req.cwd.clone(),
        });
    }
    Ok(())
}

/// Single-tenant, no isolation. See [`SandboxKind::Passthrough`].
pub struct PassthroughSandbox;

impl Sandbox for PassthroughSandbox {
    fn kind(&self) -> SandboxKind { SandboxKind::Passthrough }

    fn preflight(&self, tenant_count: usize) -> Result<(), SandboxError> {
        if tenant_count > 1 {
            return Err(SandboxError::Refused(format!(
                "passthrough provides no isolation but {tenant_count} tenants are configured; \
                 use local-uid or container"
            )));
        }
        Ok(())
    }

    fn resolve(&self, req: &SpawnRequest, paths: &TenantPaths)
        -> Result<ResolvedCommand, SandboxError>
    {
        validate(req, paths)?;
        Ok(ResolvedCommand {
            program: req.program.clone(),
            args: req.args.clone(),
            cwd: req.cwd.clone(),
            env: req.env.clone(),
        })
    }
}

/// One container per tenant.
///
/// The tenant home is bind-mounted at the same path inside the container so
/// absolute paths in agent transcripts and hive state mean the same thing on both
/// sides — otherwise every stored path would need rewriting on the way in and out.
pub struct ContainerSandbox {
    /// `podman` or `docker`.
    pub runtime: String,
    pub image: String,
}

impl Sandbox for ContainerSandbox {
    fn kind(&self) -> SandboxKind { SandboxKind::Container }

    fn preflight(&self, _tenant_count: usize) -> Result<(), SandboxError> {
        which(&self.runtime).ok_or_else(|| SandboxError::Unavailable(
            SandboxKind::Container,
            format!("{} not found on PATH", self.runtime),
        ))?;
        Ok(())
    }

    fn resolve(&self, req: &SpawnRequest, paths: &TenantPaths)
        -> Result<ResolvedCommand, SandboxError>
    {
        validate(req, paths)?;
        let home = paths.home();
        let mut args = vec![
            "run".into(), "--rm".into(), "--interactive".into(), "--tty".into(),
            // No inherited daemon socket, no extra privileges: an agent that
            // escapes the CLI still cannot reach the host's container runtime.
            "--security-opt".into(), "no-new-privileges".into(),
            "--name".into(), format!("md-{}-{}", req.tenant, short_id(&req.cwd)),
            "--volume".into(), format!("{}:{}", home.display(), home.display()),
            "--workdir".into(), req.cwd.display().to_string(),
        ];
        // Env is passed by name through the runtime rather than baked into the
        // image, so secrets never persist in a layer.
        for k in req.env.keys() {
            args.push("--env".into());
            args.push(k.clone());
        }
        args.push(self.image.clone());
        args.push(req.program.clone());
        args.extend(req.args.iter().cloned());

        Ok(ResolvedCommand {
            program: self.runtime.clone(),
            args,
            cwd: req.cwd.clone(),
            env: req.env.clone(),
        })
    }
}

fn short_id(p: &std::path::Path) -> String {
    p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "root".into())
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|p| p.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(tenant: &str, cwd: &str) -> SpawnRequest {
        SpawnRequest {
            tenant: TenantId::parse(tenant).unwrap(),
            program: "claude".into(),
            args: vec![],
            cwd: PathBuf::from(cwd),
            env: HashMap::new(),
            cols: 80, rows: 24,
        }
    }
    fn paths(tenant: &str) -> TenantPaths {
        TenantPaths::new("/srv/md", TenantId::parse(tenant).unwrap())
    }

    /// The check that keeps `Passthrough` from being a multi-tenant breach.
    #[test]
    fn passthrough_refuses_to_start_multi_tenant() {
        assert!(PassthroughSandbox.preflight(1).is_ok());
        assert!(PassthroughSandbox.preflight(2).is_err());
    }

    #[test]
    fn spawning_outside_the_tenant_home_is_refused() {
        let r = PassthroughSandbox.resolve(&req("acme", "/srv/md/other/proj"), &paths("acme"));
        assert!(matches!(r, Err(SandboxError::OutsideTenantHome { .. })));
    }

    /// A request must never be resolved against another tenant's paths, even if
    /// that other tenant's home would happily contain the cwd.
    #[test]
    fn tenant_and_paths_must_agree() {
        let r = PassthroughSandbox.resolve(&req("acme", "/srv/md/other/proj"), &paths("other"));
        assert!(matches!(r, Err(SandboxError::Refused(_))));
    }

    #[test]
    fn container_binds_the_tenant_home_and_drops_privileges() {
        let sb = ContainerSandbox { runtime: "podman".into(), image: "md/agent:0.4.6".into() };
        let cmd = sb.resolve(&req("acme", "/srv/md/acme/workspaces/p"), &paths("acme")).unwrap();
        assert_eq!(cmd.program, "podman");
        assert!(cmd.args.windows(2).any(|w| w[0] == "--volume"
            && w[1] == "/srv/md/acme:/srv/md/acme"));
        assert!(cmd.args.windows(2).any(|w| w[0] == "--security-opt"
            && w[1] == "no-new-privileges"));
        // The agent command survives intact at the end of the argv.
        assert_eq!(cmd.args.last().unwrap(), "claude");
    }
}
