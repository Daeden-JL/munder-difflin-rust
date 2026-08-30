//! Encrypted secret storage.
//!
//! The Electron original keeps integration secrets encrypted at rest through
//! `safeStorage`, and — the part that matters — **fails closed**: if encryption
//! is unavailable it refuses to store rather than falling back to plaintext.
//!
//! There is no OS keychain in a container, so the key comes from the
//! environment (`MD_SECRET_KEY`) and the same rule holds: with no key
//! configured, writing a secret FAILS. A server that quietly downgraded to
//! plaintext would be worse than one that refuses, because the operator would
//! believe the secrets were protected.
//!
//! Three invariants carried over intact:
//!   * a secret is never returned to a client — only `hasSecret: true`,
//!   * a secret is never logged, and
//!   * records carry a `secretRef` handle, never a value.
//!
//! Secrets are per tenant, in the tenant's own home. Cross-tenant reads are
//! impossible because the path is derived from the authenticated tenant, not
//! from anything a client sends.

use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, OsRng};
use chacha20poly1305::{AeadCore, Key, XChaCha20Poly1305, XNonce};
use serde_json::{json, Value};

/// Argon2 salt for deriving the data key from the operator's passphrase.
///
/// Fixed, and that is correct here: the derived key must be identical across
/// restarts or every stored secret becomes unreadable, and a per-install random
/// salt would have to live beside the ciphertext anyway. The passphrase carries
/// the entropy; this only separates our derivation from anyone else's.
const SALT: &[u8] = b"munder-difflin-secret-store-v1";

pub struct Secrets {
    path: PathBuf,
}

#[derive(Debug, PartialEq)]
pub enum SecretError {
    /// No `MD_SECRET_KEY`. Storing anyway would mean plaintext on disk.
    NoKey,
    Crypto(String),
    Io(String),
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SecretError::NoKey => write!(
                f,
                "secret storage is unavailable: set MD_SECRET_KEY (32+ characters) to enable it. \
                 Refusing to store secrets in plaintext."
            ),
            SecretError::Crypto(e) => write!(f, "secret storage failed: {e}"),
            SecretError::Io(e) => write!(f, "secret storage failed: {e}"),
        }
    }
}

/// Derive the data key from `MD_SECRET_KEY`, or report that there is none.
///
/// A short passphrase is rejected rather than stretched into a false sense of
/// security — Argon2 raises the cost of a guess, it does not create entropy
/// that was never there.
fn data_key() -> Result<Key, SecretError> {
    use argon2::Argon2;

    let pass = std::env::var("MD_SECRET_KEY").unwrap_or_default();
    if pass.trim().len() < 32 {
        return Err(SecretError::NoKey);
    }
    let mut out = [0u8; 32];
    Argon2::default()
        .hash_password_into(pass.as_bytes(), SALT, &mut out)
        .map_err(|e| SecretError::Crypto(e.to_string()))?;
    Ok(*Key::from_slice(&out))
}

/// Whether this server can store secrets at all. Surfaced so the UI can say
/// "secret storage is off" rather than showing a save button that always fails.
pub fn available() -> bool {
    data_key().is_ok()
}

impl Secrets {
    pub fn new(harness_home: &Path) -> Self {
        Self { path: harness_home.join("secrets.json") }
    }

    fn read(&self) -> Value {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| json!({}))
    }

    fn write(&self, v: &Value) -> Result<(), SecretError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| SecretError::Io(e.to_string()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(v).unwrap_or_default())
            .map_err(|e| SecretError::Io(e.to_string()))?;
        restrict(&tmp)?;
        std::fs::rename(&tmp, &self.path).map_err(|e| SecretError::Io(e.to_string()))
    }

    /// Store a secret under `reference`. Refuses when no key is configured.
    pub fn set(&self, reference: &str, plaintext: &str) -> Result<(), SecretError> {
        let key = data_key()?;
        let cipher = XChaCha20Poly1305::new(&key);
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ct = cipher
            .encrypt(&nonce, plaintext.as_bytes())
            .map_err(|e| SecretError::Crypto(e.to_string()))?;

        let mut all = self.read();
        let map = all.as_object_mut().ok_or_else(|| SecretError::Io("corrupt store".into()))?;
        map.insert(
            reference.to_string(),
            json!({ "n": hex(nonce.as_slice()), "c": hex(&ct) }),
        );
        self.write(&all)
    }

    /// Decrypt a secret. **Server-internal only** — no channel returns this to a
    /// client, which is why the type is `String` and not something serialised.
    pub fn get(&self, reference: &str) -> Result<Option<String>, SecretError> {
        let key = data_key()?;
        let all = self.read();
        let Some(rec) = all.get(reference) else { return Ok(None) };

        let (Some(n), Some(c)) = (
            rec.get("n").and_then(|v| v.as_str()).and_then(unhex),
            rec.get("c").and_then(|v| v.as_str()).and_then(unhex),
        ) else {
            return Err(SecretError::Crypto("malformed record".into()));
        };
        if n.len() != 24 {
            return Err(SecretError::Crypto("malformed nonce".into()));
        }

        let plain = XChaCha20Poly1305::new(&key)
            .decrypt(XNonce::from_slice(&n), c.as_ref())
            // A failure here is authentication failing — a wrong key or a
            // tampered file. Both are reported the same way, because telling
            // them apart tells an attacker which one they achieved.
            .map_err(|_| SecretError::Crypto("could not decrypt (wrong key or altered store)".into()))?;
        Ok(Some(String::from_utf8_lossy(&plain).into_owned()))
    }

    /// Whether a secret exists. This — not the value — is what a client sees.
    pub fn has(&self, reference: &str) -> bool {
        self.read().get(reference).is_some()
    }

    pub fn remove(&self, reference: &str) -> Result<bool, SecretError> {
        let mut all = self.read();
        let existed = all
            .as_object_mut()
            .map(|m| m.remove(reference).is_some())
            .unwrap_or(false);
        if existed {
            self.write(&all)?;
        }
        Ok(existed)
    }
}

/// Owner-only. The file holds ciphertext, but its mode is the second lock: a
/// key that leaks separately should not also hand over the data.
fn restrict(path: &Path) -> Result<(), SecretError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| SecretError::Io(e.to_string()))?;
    }
    let _ = path;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    s.len().is_multiple_of(2)
        .then(|| {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
                .collect::<Option<Vec<u8>>>()
        })
        .flatten()
}

/// `MD_SECRET_KEY` is process-wide, so any test that sets or clears it must
/// hold this — including tests in other modules that store a secret. Without it
/// one module's cleanup clears the key another module is mid-way through using.
#[cfg(test)]
pub(crate) fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    static L: std::sync::Mutex<()> = std::sync::Mutex::new(());
    L.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        env_guard()
    }

    fn store() -> (Secrets, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "md-sec-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (Secrets::new(&dir), dir)
    }

    /// The invariant the original is built around: no key means no storage, NOT
    /// plaintext storage. An operator who is told a secret was saved must be
    /// right about that.
    #[test]
    fn without_a_key_storing_fails_rather_than_writing_plaintext() {
        let _g = guard();
        std::env::remove_var("MD_SECRET_KEY");
        let (s, dir) = store();

        assert_eq!(s.set("ref", "hunter2"), Err(SecretError::NoKey));
        assert!(!available());
        assert!(!dir.join("secrets.json").exists(), "nothing may be written");
    }

    /// Argon2 raises the cost of a guess; it does not create entropy that was
    /// never there.
    #[test]
    fn a_short_passphrase_is_refused_not_stretched() {
        let _g = guard();
        std::env::set_var("MD_SECRET_KEY", "short");
        assert_eq!(data_key().unwrap_err(), SecretError::NoKey);
        std::env::remove_var("MD_SECRET_KEY");
    }

    #[test]
    fn a_secret_round_trips_and_never_appears_in_the_file() {
        let _g = guard();
        std::env::set_var("MD_SECRET_KEY", "a".repeat(40));
        let (s, dir) = store();

        s.set("slack", "xoxb-super-secret-token").unwrap();
        assert_eq!(s.get("slack").unwrap().as_deref(), Some("xoxb-super-secret-token"));
        assert!(s.has("slack"));
        assert!(!s.has("nothing"));

        let raw = std::fs::read_to_string(dir.join("secrets.json")).unwrap();
        assert!(!raw.contains("xoxb"), "the plaintext must not be on disk");

        assert!(s.remove("slack").unwrap());
        assert!(!s.has("slack"));
        assert!(!s.remove("slack").unwrap());
        std::env::remove_var("MD_SECRET_KEY");
    }

    /// A tampered ciphertext must fail authentication, not decrypt to garbage.
    #[test]
    fn an_altered_store_fails_to_decrypt() {
        let _g = guard();
        std::env::set_var("MD_SECRET_KEY", "b".repeat(40));
        let (s, dir) = store();
        s.set("k", "value").unwrap();

        let mut all: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("secrets.json")).unwrap()).unwrap();
        let c = all["k"]["c"].as_str().unwrap().to_string();
        // Flip one ciphertext byte.
        let flipped = format!("{}{}", if c.starts_with('a') { "b" } else { "a" }, &c[1..]);
        all["k"]["c"] = json!(flipped);
        std::fs::write(dir.join("secrets.json"), all.to_string()).unwrap();

        assert!(matches!(s.get("k"), Err(SecretError::Crypto(_))));
        std::env::remove_var("MD_SECRET_KEY");
    }

    /// A different key must not silently return nothing — that would look like
    /// "no secret stored" and invite overwriting a good one.
    #[test]
    fn a_wrong_key_reports_failure_rather_than_absence() {
        let _g = guard();
        std::env::set_var("MD_SECRET_KEY", "c".repeat(40));
        let (s, _dir) = store();
        s.set("k", "value").unwrap();

        std::env::set_var("MD_SECRET_KEY", "d".repeat(40));
        assert!(matches!(s.get("k"), Err(SecretError::Crypto(_))));
        assert!(s.has("k"), "the record still exists");
        std::env::remove_var("MD_SECRET_KEY");
    }

    #[cfg(unix)]
    #[test]
    fn the_store_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let _g = guard();
        std::env::set_var("MD_SECRET_KEY", "e".repeat(40));
        let (s, dir) = store();
        s.set("k", "v").unwrap();
        let mode = std::fs::metadata(dir.join("secrets.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::env::remove_var("MD_SECRET_KEY");
    }
}
