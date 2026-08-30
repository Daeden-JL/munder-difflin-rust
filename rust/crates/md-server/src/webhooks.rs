//! Inbound webhooks: external POSTs become hive work, and the caller can poll
//! that work by a capability token.
//!
//! **Many endpoints, one server.** Endpoints are told apart by the id in the
//! path, so adding one costs nothing. In Electron this needed its own HTTP
//! server and a tunnel; here the harness is already a web server, so these are
//! just two more routes — the tunnel disappears entirely.
//!
//! This is a PUBLIC surface, so the gate is strict, and each property below is
//! load-bearing rather than incidental:
//!
//! * **Constant-time comparison** against that endpoint's secret only, so
//!   revoking one endpoint cannot affect another.
//! * **An unknown endpoint answers exactly like a wrong secret** — the compare
//!   still runs, against an unguessable per-process decoy, and the reply is
//!   byte-identical. Otherwise the surface can be walked to discover which
//!   endpoints (and tenants) exist.
//! * **Secrets are never logged, echoed, or forwarded** into the routed message,
//!   the card, or the response.
//! * **The capability token is 192-bit and stored only as a SHA-256 hash.** A
//!   GET reveals the single task that token maps to and nothing else — no
//!   listing, and a leak of the task ledger does not leak the tokens.
//! * **Body cap and fixed-window rate limits, global and per-endpoint**, so one
//!   noisy caller cannot starve the others. Both bound abuse BEFORE parsing or
//!   any cryptographic work.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Largest inbound body. Checked before parsing: the point is to bound work an
/// unauthenticated caller can cause.
pub const MAX_BODY: usize = 64 * 1024;

/// Fixed-window rate limits. The global window stops a flood; the per-endpoint
/// window stops one caller starving the others.
const WINDOW: Duration = Duration::from_secs(60);
const GLOBAL_PER_WINDOW: u32 = 600;
const PER_ENDPOINT_PER_WINDOW: u32 = 120;

/// Compared against when the endpoint does not exist, so an unknown id costs the
/// same work and yields the same answer as a wrong secret. Random per process:
/// a fixed decoy could be discovered and used to distinguish the two cases.
fn decoy() -> &'static str {
    static D: OnceLock<String> = OnceLock::new();
    D.get_or_init(|| random_hex(32))
}

pub fn random_hex(bytes: usize) -> String {
    use rand::RngCore;
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// 192 bits, which is what makes the token unguessable rather than merely long.
pub fn mint_token() -> String {
    random_hex(24)
}

pub fn hash_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time comparison over the CONTENT.
///
/// A short-circuiting `==` leaks the length of the matching prefix through
/// timing, which is enough to recover a secret one byte at a time. This
/// accumulates every difference and never returns early.
///
/// A length mismatch returns immediately, which is deliberate: the length of a
/// secret is not itself secret, and every secret this compares is a fixed-width
/// generated token. Guarding it would buy nothing and obscure the part that
/// matters.
pub fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[derive(Default)]
struct Window {
    started: Option<Instant>,
    count: u32,
}

impl Window {
    fn allow(&mut self, limit: u32) -> bool {
        let now = Instant::now();
        match self.started {
            Some(t) if now.duration_since(t) < WINDOW => {}
            _ => {
                self.started = Some(now);
                self.count = 0;
            }
        }
        self.count += 1;
        self.count <= limit
    }
}

#[derive(Default)]
struct Limits {
    global: Window,
    per_endpoint: HashMap<String, Window>,
}

fn limits() -> &'static Mutex<Limits> {
    static L: OnceLock<Mutex<Limits>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(Limits::default()))
}

/// Both windows are consumed on every request, including rejected ones — a
/// caller guessing secrets must be rate-limited too.
pub fn allow(key: &str) -> bool {
    let mut l = limits().lock().unwrap();
    let global_ok = l.global.allow(GLOBAL_PER_WINDOW);
    let endpoint_ok = l
        .per_endpoint
        .entry(key.to_string())
        .or_default()
        .allow(PER_ENDPOINT_PER_WINDOW);
    global_ok && endpoint_ok
}

pub struct Webhooks {
    config: std::path::PathBuf,
}

impl Webhooks {
    pub fn new(harness_home: &std::path::Path) -> Self {
        Self { config: harness_home.join("config.json") }
    }

    fn config(&self) -> Value {
        std::fs::read_to_string(&self.config)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}))
    }

    /// The stored endpoints, secrets included. Internal only — `list` is what a
    /// client sees.
    fn records(&self) -> Vec<Value> {
        self.config()["webhooks"].as_array().cloned().unwrap_or_default()
    }

    /// The client-facing view: everything except the secret, plus whether one is
    /// set. A secret is shown ONCE, when it is generated, and never again.
    pub fn list(&self) -> Value {
        json!(self
            .records()
            .into_iter()
            .map(|mut r| {
                if let Some(m) = r.as_object_mut() {
                    let has = m.remove("secret").and_then(|v| v.as_str().map(|s| !s.is_empty()));
                    m.insert("hasSecret".into(), json!(has.unwrap_or(false)));
                }
                r
            })
            .collect::<Vec<Value>>())
    }

    /// Replace the endpoint list.
    ///
    /// A saved endpoint that omits its secret KEEPS the stored one: the client
    /// never receives secrets, so it cannot send them back, and a naive
    /// wholesale write would silently disable every endpoint the moment someone
    /// renamed one.
    pub fn save(&self, incoming: &[Value]) -> Value {
        let existing = self.records();
        let secret_of = |id: &str| -> Option<String> {
            existing
                .iter()
                .find(|r| r["id"].as_str() == Some(id))
                .and_then(|r| r["secret"].as_str().map(String::from))
        };

        let mut out = Vec::new();
        for r in incoming {
            let Some(id) = r["id"].as_str().filter(|s| !s.is_empty()) else { continue };
            if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                continue;
            }
            let mut rec = r.clone();
            if let Some(m) = rec.as_object_mut() {
                m.remove("hasSecret");
                let incoming_secret = r["secret"].as_str().filter(|s| !s.is_empty()).map(String::from);
                match incoming_secret.or_else(|| secret_of(id)) {
                    Some(s) => {
                        m.insert("secret".into(), json!(s));
                    }
                    None => {
                        m.remove("secret");
                    }
                }
            }
            out.push(rec);
        }
        self.write(out);
        self.list()
    }

    pub fn delete(&self, id: &str) -> Value {
        let kept: Vec<Value> = self
            .records()
            .into_iter()
            .filter(|r| r["id"].as_str() != Some(id))
            .collect();
        self.write(kept);
        self.list()
    }

    fn write(&self, records: Vec<Value>) {
        let mut cfg = self.config();
        cfg["webhooks"] = json!(records);
        if let Some(parent) = self.config.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&self.config, serde_json::to_vec_pretty(&cfg).unwrap_or_default());
    }

    /// Check a presented secret against an endpoint.
    ///
    /// Returns the endpoint's PUBLIC fields on success — the caller is handed
    /// `{id, name}`, never the record, so a secret cannot reach the routed
    /// message or the response by accident.
    pub fn authenticate(&self, id: &str, presented: &str) -> Option<Value> {
        let records = self.records();
        let found = records.iter().find(|r| r["id"].as_str() == Some(id));

        // An unknown endpoint still runs a comparison, against the decoy, so it
        // is indistinguishable from a wrong secret.
        let stored = found
            .and_then(|r| r["secret"].as_str())
            .unwrap_or_else(|| decoy());
        if !secret_eq(stored, presented) {
            return None;
        }
        let rec = found?;
        // An endpoint with no secret configured is not open — it is unusable.
        rec["secret"].as_str().filter(|s| !s.is_empty())?;
        if !rec["enabled"].as_bool().unwrap_or(true) {
            return None;
        }
        Some(json!({ "id": rec["id"], "name": rec["name"] }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (Webhooks, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "md-wh-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (Webhooks::new(&dir), dir)
    }

    #[test]
    fn comparison_is_length_safe_and_correct() {
        assert!(secret_eq("abc", "abc"));
        assert!(!secret_eq("abc", "abd"));
        assert!(!secret_eq("abc", "abcd"));
        assert!(!secret_eq("", "x"));
        assert!(secret_eq("", ""));
    }

    #[test]
    fn a_token_is_192_bit_and_stored_only_as_a_hash() {
        let t = mint_token();
        assert_eq!(t.len(), 48, "24 bytes of hex");
        assert_ne!(mint_token(), mint_token());

        let h = hash_token(&t);
        assert_eq!(h.len(), 64);
        assert_ne!(h, t, "the raw token must never be what is stored");
        assert_eq!(h, hash_token(&t), "hashing is stable");
    }

    /// The client never receives a secret, so it cannot send one back — a naive
    /// wholesale write would disable every endpoint on the first rename.
    #[test]
    fn saving_without_a_secret_keeps_the_stored_one() {
        let (w, _d) = store();
        w.save(&[json!({ "id": "ci", "name": "CI", "secret": "s3cret", "enabled": true })]);
        assert_eq!(w.list()[0]["hasSecret"], true);

        // The client round-trips the view it was given, which has no secret.
        let view = w.list();
        w.save(view.as_array().unwrap());
        assert_eq!(w.list()[0]["hasSecret"], true, "the secret survived the round trip");
        assert!(w.authenticate("ci", "s3cret").is_some());
    }

    #[test]
    fn a_secret_never_appears_in_the_client_view() {
        let (w, _d) = store();
        w.save(&[json!({ "id": "ci", "name": "CI", "secret": "s3cret" })]);
        let view = w.list().to_string();
        assert!(!view.contains("s3cret"));
        assert!(view.contains("hasSecret"));
    }

    /// The gate returns only public fields, so a secret cannot reach the routed
    /// message or the response by accident.
    #[test]
    fn authentication_returns_public_fields_only() {
        let (w, _d) = store();
        w.save(&[json!({ "id": "ci", "name": "CI", "secret": "s3cret", "enabled": true })]);

        let ok = w.authenticate("ci", "s3cret").unwrap();
        assert_eq!(ok["id"], "ci");
        assert_eq!(ok["name"], "CI");
        assert!(ok.get("secret").is_none());

        assert!(w.authenticate("ci", "wrong").is_none());
        assert!(w.authenticate("nope", "s3cret").is_none(), "unknown id");
    }

    /// An endpoint with no secret is unusable, not open to everyone.
    #[test]
    fn an_endpoint_without_a_secret_cannot_be_called() {
        let (w, _d) = store();
        w.save(&[json!({ "id": "open", "name": "Open" })]);
        assert!(w.authenticate("open", "").is_none());
        assert!(w.authenticate("open", "anything").is_none());
    }

    #[test]
    fn a_disabled_endpoint_refuses_a_correct_secret() {
        let (w, _d) = store();
        w.save(&[json!({ "id": "ci", "secret": "s", "enabled": false })]);
        assert!(w.authenticate("ci", "s").is_none());
    }

    #[test]
    fn ids_that_could_traverse_a_path_are_not_saved() {
        let (w, _d) = store();
        w.save(&[
            json!({ "id": "../etc", "secret": "s" }),
            json!({ "id": "ok-1", "secret": "s" }),
            json!({ "name": "no id" }),
        ]);
        let list = w.list();
        let ids: Vec<&str> = list.as_array().unwrap().iter()
            .map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["ok-1"]);
    }

    #[test]
    fn deleting_one_endpoint_leaves_the_others_callable() {
        let (w, _d) = store();
        w.save(&[
            json!({ "id": "a", "secret": "sa", "enabled": true }),
            json!({ "id": "b", "secret": "sb", "enabled": true }),
        ]);
        w.delete("a");
        assert!(w.authenticate("a", "sa").is_none());
        assert!(w.authenticate("b", "sb").is_some(), "revoking one must not affect another");
    }

    /// Rejected requests consume the budget too — a caller guessing secrets has
    /// to be rate-limited, not just a successful one.
    #[test]
    fn the_rate_limit_bounds_a_single_endpoint() {
        let key = format!("test-{}", random_hex(8));
        let mut allowed = 0;
        for _ in 0..PER_ENDPOINT_PER_WINDOW + 20 {
            if allow(&key) {
                allowed += 1;
            }
        }
        assert_eq!(allowed, PER_ENDPOINT_PER_WINDOW);
    }
}
