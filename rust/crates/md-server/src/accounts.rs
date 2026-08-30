//! Accounts and roles, persisted.
//!
//! Until now there was exactly one account, from environment variables, held in
//! memory. That is fine for a test box and wrong for anything shared: it cannot
//! be changed without a restart, cannot be revoked, and gives everyone who has
//! it the same power.
//!
//! Two roles, deliberately, because a third would need a reason:
//!
//! * **admin** — manages accounts, and has a tenant like anyone else.
//! * **member** — has a tenant, and nothing else.
//!
//! Roles gate ACCOUNT MANAGEMENT only. They are not a second tenancy check:
//! tenant isolation is enforced by the extractor on every request, and an admin
//! is not thereby able to read another tenant's files. An admin who wants
//! another tenant's data has to give themselves an account in it — which is a
//! visible, logged act rather than an invisible capability.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use md_tenant::TenantId;
use serde::{Deserialize, Serialize};

/// Minimum password length for an account created through the API.
///
/// Short enough not to be obstructive, long enough that Argon2's work factor is
/// defending something. The env-var bootstrap account is exempt — it is the
/// operator's own choice, made before the server was running.
const MIN_PASSWORD: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Admin,
    Member,
}

impl Role {
    pub fn is_admin(self) -> bool {
        self == Role::Admin
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Account {
    pub user: String,
    pub tenant: String,
    pub role: Role,
    /// Argon2id. The hash string carries its own salt and parameters, so there
    /// is nothing else to store and nothing to get wrong reconstructing it.
    #[serde(rename = "hash")]
    pub password_hash: String,
    /// A disabled account keeps its history but cannot sign in. Deleting would
    /// lose the record of who did what.
    #[serde(default)]
    pub disabled: bool,
}

impl Account {
    pub fn new(user: &str, tenant: &str, role: Role, password: &str) -> Result<Self, String> {
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| format!("hashing failed: {e}"))?
            .to_string();
        Ok(Self {
            user: user.to_string(),
            tenant: tenant.to_string(),
            role,
            password_hash: hash,
            disabled: false,
        })
    }

    pub fn verify(&self, password: &str) -> bool {
        if self.disabled {
            return false;
        }
        PasswordHash::new(&self.password_hash)
            .map(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
            .unwrap_or(false)
    }

    /// The client-facing view. There is no shape in which a hash is useful to a
    /// browser, so it never leaves.
    pub fn view(&self) -> serde_json::Value {
        serde_json::json!({
            "user": self.user,
            "tenant": self.tenant,
            "role": self.role,
            "disabled": self.disabled,
        })
    }
}

/// The account store, on disk beside the tenant data.
pub struct Accounts {
    path: PathBuf,
    inner: std::sync::RwLock<HashMap<String, Account>>,
}

fn valid_user(u: &str) -> bool {
    !u.is_empty()
        && u.len() <= 64
        && u.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

impl Accounts {
    /// Load from disk, seeding from the environment when empty.
    ///
    /// The env-var account is a BOOTSTRAP, not the model: it exists so a fresh
    /// server can be signed into at all. Once a stored account exists the
    /// environment is ignored, or an operator could never revoke it.
    pub fn load(data_root: &Path, seed: Option<(String, String, String)>) -> Self {
        let path = data_root.join("accounts.json");
        let mut map: HashMap<String, Account> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Vec<Account>>(&t).ok())
            .map(|list| list.into_iter().map(|a| (a.user.clone(), a)).collect())
            .unwrap_or_default();

        if map.is_empty() {
            if let Some((user, tenant, password)) = seed {
                // The first account is an admin, or nobody could ever create
                // the second one.
                match Account::new(&user, &tenant, Role::Admin, &password) {
                    Ok(a) => {
                        tracing::info!(%user, "seeded the first account from the environment");
                        map.insert(user, a);
                    }
                    Err(e) => tracing::error!("could not seed an account: {e}"),
                }
            }
        }

        let store = Self { path, inner: std::sync::RwLock::new(map) };
        store.save();
        store
    }

    fn save(&self) {
        let list: Vec<Account> = self.inner.read().unwrap().values().cloned().collect();
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Write-then-rename, and owner-only: this file holds every password
        // hash on the server.
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(&list).unwrap_or_default()).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
            }
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    pub fn get(&self, user: &str) -> Option<Account> {
        self.inner.read().unwrap().get(user).cloned()
    }

    pub fn tenants(&self) -> Vec<TenantId> {
        let mut out: Vec<TenantId> = self
            .inner
            .read()
            .unwrap()
            .values()
            .filter_map(|a| TenantId::parse(&a.tenant).ok())
            .collect();
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        out.dedup_by(|a, b| a.as_str() == b.as_str());
        out
    }

    pub fn list(&self) -> Vec<serde_json::Value> {
        let mut out: Vec<Account> = self.inner.read().unwrap().values().cloned().collect();
        out.sort_by(|a, b| a.user.cmp(&b.user));
        out.iter().map(Account::view).collect()
    }

    pub fn create(&self, user: &str, tenant: &str, role: Role, password: &str) -> Result<(), String> {
        if !valid_user(user) {
            return Err("a username may contain letters, digits, - _ and .".into());
        }
        if TenantId::parse(tenant).is_err() {
            return Err("invalid tenant id".into());
        }
        if password.len() < MIN_PASSWORD {
            return Err(format!("the password must be at least {MIN_PASSWORD} characters"));
        }
        let mut g = self.inner.write().unwrap();
        if g.contains_key(user) {
            return Err("that username is taken".into());
        }
        g.insert(user.to_string(), Account::new(user, tenant, role, password)?);
        drop(g);
        self.save();
        Ok(())
    }

    /// Change a password. `actor` is who is asking: an admin may reset anyone's,
    /// and anyone may change their own — but nobody else's.
    pub fn set_password(&self, actor: &Account, user: &str, password: &str) -> Result<(), String> {
        if !actor.role.is_admin() && actor.user != user {
            return Err("you may only change your own password".into());
        }
        if password.len() < MIN_PASSWORD {
            return Err(format!("the password must be at least {MIN_PASSWORD} characters"));
        }
        let mut g = self.inner.write().unwrap();
        let Some(a) = g.get_mut(user) else { return Err("no such account".into()) };
        a.password_hash = Account::new(user, &a.tenant, a.role, password)?.password_hash;
        drop(g);
        self.save();
        Ok(())
    }

    /// Disable or re-enable an account.
    ///
    /// Refuses to disable the LAST enabled admin. Locking every administrator
    /// out of a running server is not a state anyone can undo from inside it.
    pub fn set_disabled(&self, user: &str, disabled: bool) -> Result<(), String> {
        let mut g = self.inner.write().unwrap();
        let Some(target) = g.get(user).cloned() else { return Err("no such account".into()) };
        if disabled && target.role.is_admin() {
            let others = g
                .values()
                .filter(|a| a.role.is_admin() && !a.disabled && a.user != user)
                .count();
            if others == 0 {
                return Err("this is the last active admin — promote another first".into());
            }
        }
        g.get_mut(user).unwrap().disabled = disabled;
        drop(g);
        self.save();
        Ok(())
    }

    /// Change a role, with the same last-admin guard for the same reason.
    pub fn set_role(&self, user: &str, role: Role) -> Result<(), String> {
        let mut g = self.inner.write().unwrap();
        let Some(target) = g.get(user).cloned() else { return Err("no such account".into()) };
        if target.role.is_admin() && !role.is_admin() {
            let others = g
                .values()
                .filter(|a| a.role.is_admin() && !a.disabled && a.user != user)
                .count();
            if others == 0 {
                return Err("this is the last active admin — promote another first".into());
            }
        }
        g.get_mut(user).unwrap().role = role;
        drop(g);
        self.save();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Accounts, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "md-acct-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let a = Accounts::load(&dir, Some(("root".into(), "ops".into(), "a-long-enough-secret".into())));
        (a, dir)
    }

    /// The first account has to be an admin, or nobody could ever create the
    /// second one.
    #[test]
    fn the_seeded_account_is_an_admin() {
        let (a, _d) = store();
        let root = a.get("root").unwrap();
        assert!(root.role.is_admin());
        assert!(root.verify("a-long-enough-secret"));
        assert!(!root.verify("wrong"));
    }

    /// The environment is a bootstrap, not the model. Once an account exists it
    /// must be ignored, or an operator could never revoke it.
    #[test]
    fn the_environment_seed_is_ignored_once_accounts_exist() {
        let (a, dir) = store();
        a.create("someone", "ops", Role::Member, "another-long-secret").unwrap();
        drop(a);

        let reloaded = Accounts::load(&dir, Some(("intruder".into(), "ops".into(), "yet-another-secret".into())));
        assert!(reloaded.get("intruder").is_none(), "the seed must not re-apply");
        assert!(reloaded.get("someone").is_some(), "stored accounts survive a reload");
    }

    #[test]
    fn a_password_hash_never_reaches_a_client() {
        let (a, _d) = store();
        let view = a.list();
        let text = serde_json::to_string(&view).unwrap();
        assert!(!text.contains("$argon2"), "the hash leaked into the view");
        assert!(text.contains("\"role\":\"admin\""));
    }

    #[test]
    fn account_creation_validates_its_inputs() {
        let (a, _d) = store();
        assert!(a.create("ok-user", "ops", Role::Member, "a-long-enough-secret").is_ok());
        assert!(a.create("ok-user", "ops", Role::Member, "a-long-enough-secret").is_err(), "duplicate");
        assert!(a.create("bad user", "ops", Role::Member, "a-long-enough-secret").is_err(), "space");
        assert!(a.create("../etc", "ops", Role::Member, "a-long-enough-secret").is_err(), "traversal");
        assert!(a.create("fine", "Bad Tenant", Role::Member, "a-long-enough-secret").is_err(), "tenant");
        assert!(a.create("fine", "ops", Role::Member, "short").is_err(), "weak password");
    }

    /// Locking every administrator out of a running server is not a state
    /// anyone can undo from inside it.
    #[test]
    fn the_last_admin_cannot_be_disabled_or_demoted() {
        let (a, _d) = store();
        assert!(a.set_disabled("root", true).is_err());
        assert!(a.set_role("root", Role::Member).is_err());

        a.create("second", "ops", Role::Admin, "a-long-enough-secret").unwrap();
        // With another admin present, both operations are allowed.
        assert!(a.set_role("root", Role::Member).is_ok());
        assert!(a.set_disabled("second", true).is_err(), "now IT is the last one");
    }

    #[test]
    fn a_disabled_account_cannot_sign_in_but_keeps_its_record() {
        let (a, _d) = store();
        a.create("temp", "ops", Role::Member, "a-long-enough-secret").unwrap();
        a.set_disabled("temp", true).unwrap();
        assert!(!a.get("temp").unwrap().verify("a-long-enough-secret"));
        assert!(a.get("temp").is_some(), "the record survives");
    }

    /// Anyone may change their own password; only an admin may change another's.
    #[test]
    fn password_changes_are_self_or_admin_only() {
        let (a, _d) = store();
        a.create("alice", "ops", Role::Member, "a-long-enough-secret").unwrap();
        a.create("bob", "ops", Role::Member, "a-long-enough-secret").unwrap();
        let alice = a.get("alice").unwrap();
        let root = a.get("root").unwrap();

        assert!(a.set_password(&alice, "alice", "her-own-new-secret").is_ok());
        assert!(a.set_password(&alice, "bob", "not-hers-to-set").is_err());
        assert!(a.set_password(&root, "bob", "an-admin-reset-secret").is_ok());
        assert!(a.get("bob").unwrap().verify("an-admin-reset-secret"));
    }

    #[cfg(unix)]
    #[test]
    fn the_account_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (_a, dir) = store();
        let mode = std::fs::metadata(dir.join("accounts.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "this file holds every password hash");
    }
}
