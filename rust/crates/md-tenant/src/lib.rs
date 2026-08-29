//! Tenancy and the execution sandbox.
//!
//! # Why this crate exists before any agent code
//!
//! The product spawns arbitrary CLI agents (`claude`, `codex`, `grok`, …) with
//! filesystem access. On a single-user desktop that is exactly what the user
//! asked for. On a shared server it is remote code execution as the server user,
//! and one tenant's agent can read every other tenant's source, secrets, and
//! agent transcripts.
//!
//! Path prefixing does not fix that. An agent runs a real process; it can call
//! `open("/etc/passwd")` or `../../` its way out of any prefix the application
//! layer invents. The boundary has to be enforced by the OS, so every spawn goes
//! through [`Sandbox`] and the application layer is never trusted to confine a
//! child by convention.

use std::path::{Path, PathBuf};

pub mod sandbox;
pub use sandbox::{Sandbox, SandboxKind, SpawnRequest};

/// Opaque tenant handle. Constructed only by [`TenantId::parse`], which enforces
/// the charset, so a tenant id can never contain a path separator and be
/// interpolated into a filesystem path as `../another-tenant`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct TenantId(String);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TenantIdError {
    #[error("tenant id must be 1..=64 characters, got {0}")]
    Length(usize),
    #[error("tenant id may only contain [a-z0-9_-], found {0:?}")]
    Charset(char),
    #[error("tenant id may not start with '-' or '_'")]
    LeadingPunctuation,
}

impl TenantId {
    /// Lowercase alphanumerics plus `-` and `_`. Deliberately narrow: this string
    /// becomes a directory name, a unix username, and a container name, and the
    /// intersection of what those three accept is small.
    pub fn parse(s: &str) -> Result<Self, TenantIdError> {
        if s.is_empty() || s.len() > 64 {
            return Err(TenantIdError::Length(s.len()));
        }
        if s.starts_with('-') || s.starts_with('_') {
            return Err(TenantIdError::LeadingPunctuation);
        }
        if let Some(c) = s.chars().find(|c| !matches!(c, 'a'..='z' | '0'..='9' | '-' | '_')) {
            return Err(TenantIdError::Charset(c));
        }
        Ok(TenantId(s.to_string()))
    }

    pub fn as_str(&self) -> &str { &self.0 }
}

impl std::fmt::Display for TenantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
}

/// Where one tenant's state lives on disk.
///
/// Mirrors the single-user harness home the Electron app uses, one level down
/// under a per-tenant root, so the existing on-disk layout ports unchanged and an
/// existing install can be moved in without conversion.
#[derive(Debug, Clone)]
pub struct TenantPaths {
    root: PathBuf,
    id: TenantId,
}

impl TenantPaths {
    pub fn new(root: impl Into<PathBuf>, id: TenantId) -> Self {
        Self { root: root.into(), id }
    }

    /// `<root>/<tenant>` — the tenant's private world.
    pub fn home(&self) -> PathBuf { self.root.join(self.id.as_str()) }
    /// Harness home: config, roster, hive state.
    pub fn harness_home(&self) -> PathBuf { self.home().join(".munder-difflin") }
    pub fn hive_root(&self) -> PathBuf { self.harness_home().join("hive") }
    pub fn roster_file(&self) -> PathBuf { self.harness_home().join("roster.json") }
    pub fn config_file(&self) -> PathBuf { self.harness_home().join("config.json") }
    /// Per-tenant sqlite for the memory/knowledge layer.
    pub fn memory_db(&self) -> PathBuf { self.harness_home().join("memory.db") }
    /// Workspaces the tenant's agents may be pointed at.
    pub fn workspaces(&self) -> PathBuf { self.home().join("workspaces") }

    pub fn id(&self) -> &TenantId { &self.id }

    /// Defence in depth, not the boundary itself.
    ///
    /// The real boundary is the sandbox: this only stops the *server* from being
    /// tricked into reading across tenants by a crafted RPC argument. It cannot
    /// constrain a spawned agent, which is why it is not the only check.
    ///
    /// Rejects any path that escapes the tenant home once `..` is resolved
    /// lexically. Lexical is correct here: the check runs on paths that may not
    /// exist yet (a file about to be created), so it cannot canonicalize.
    pub fn contains(&self, path: &Path) -> bool {
        let home = normalize(&self.home());
        let candidate = if path.is_absolute() {
            normalize(path)
        } else {
            normalize(&self.home().join(path))
        };
        candidate.starts_with(&home)
    }
}

/// Resolve `.` and `..` without touching the filesystem.
///
/// `Path::canonicalize` would follow symlinks and require existence; neither is
/// wanted. A symlink out of the tenant home is the sandbox's problem, not this
/// function's, and pretending to solve it here would be worse than not trying.
fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => { out.pop(); }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_ids_that_could_traverse_or_collide() {
        assert!(TenantId::parse("acme").is_ok());
        assert!(TenantId::parse("acme-2_b").is_ok());
        assert_eq!(TenantId::parse("../etc"), Err(TenantIdError::Charset('.')));
        assert_eq!(TenantId::parse("a/b"), Err(TenantIdError::Charset('/')));
        assert_eq!(TenantId::parse("Acme"), Err(TenantIdError::Charset('A')));
        assert_eq!(TenantId::parse(""), Err(TenantIdError::Length(0)));
        assert_eq!(TenantId::parse("-x"), Err(TenantIdError::LeadingPunctuation));
    }

    #[test]
    fn containment_rejects_traversal_out_of_the_tenant_home() {
        let p = TenantPaths::new("/srv/md", TenantId::parse("acme").unwrap());
        assert!(p.contains(Path::new("/srv/md/acme/workspaces/proj")));
        assert!(p.contains(Path::new("workspaces/proj")));
        assert!(!p.contains(Path::new("/srv/md/other/secrets")));
        assert!(!p.contains(Path::new("/srv/md/acme/../other/secrets")));
        assert!(!p.contains(Path::new("../other")));
        assert!(!p.contains(Path::new("/etc/passwd")));
    }

    /// A tenant named as a prefix of another must not match it: `acme` and
    /// `acme-corp` are different tenants and `starts_with` on raw strings would
    /// conflate them. Comparing by path component avoids that.
    #[test]
    fn prefix_named_tenants_do_not_overlap() {
        let acme = TenantPaths::new("/srv/md", TenantId::parse("acme").unwrap());
        assert!(!acme.contains(Path::new("/srv/md/acme-corp/secrets")));
    }
}
