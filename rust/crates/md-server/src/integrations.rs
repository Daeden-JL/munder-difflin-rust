//! The integrations registry: what a floor may reach, and with what credential.
//!
//! Two halves, deliberately separate — the same split the Electron original
//! makes. **Metadata** lives in the tenant's config as plain records. **Secrets**
//! live in the encrypted store (`secrets.rs`) and are referenced by handle.
//!
//! The contract this exists to keep: **a secret value never crosses the wire in
//! either direction after it is set.** A client writes one and thereafter sees
//! only `hasSecret: true`. Nothing here returns a value, logs one, or puts one
//! in an error message.

use serde_json::{json, Value};

use crate::secrets::Secrets;

/// Auth types that require a stored credential. A record using one of these is
/// not usable until a secret is set, which is what `hasSecret` reports.
const NEEDS_SECRET: [&str; 3] = ["bearer", "header", "basic"];

/// Ready-made records for the services people actually wire up. Templates carry
/// no credential — they are a starting shape, not an account.
pub fn templates() -> Value {
    json!([
        { "id": "slack", "name": "Slack", "baseUrl": "https://slack.com/api",
          "authType": "bearer", "docs": "https://api.slack.com/web" },
        { "id": "github", "name": "GitHub", "baseUrl": "https://api.github.com",
          "authType": "bearer", "docs": "https://docs.github.com/rest" },
        { "id": "linear", "name": "Linear", "baseUrl": "https://api.linear.app/graphql",
          "authType": "header", "headerName": "Authorization",
          "docs": "https://developers.linear.app" },
        { "id": "generic", "name": "Generic HTTP", "baseUrl": "", "authType": "none", "docs": "" },
    ])
}

pub struct Integrations {
    config: std::path::PathBuf,
    secrets: Secrets,
}

fn secret_ref(id: &str) -> String {
    format!("integration:{id}")
}

fn needs_secret(auth: &str) -> bool {
    NEEDS_SECRET.contains(&auth)
}

impl Integrations {
    pub fn new(harness_home: &std::path::Path) -> Self {
        Self {
            config: harness_home.join("config.json"),
            secrets: Secrets::new(harness_home),
        }
    }

    fn config(&self) -> Value {
        std::fs::read_to_string(&self.config)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}))
    }

    fn save_records(&self, records: Vec<Value>) -> std::io::Result<()> {
        let mut cfg = self.config();
        cfg["integrations"] = json!(records);
        if let Some(parent) = self.config.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.config, serde_json::to_vec_pretty(&cfg).unwrap_or_default())
    }

    fn records(&self) -> Vec<Value> {
        self.config()["integrations"].as_array().cloned().unwrap_or_default()
    }

    /// The client-facing view. `secretRef` is stripped and replaced with a
    /// boolean — the handle is an internal detail, and echoing it back would
    /// invite a client to address the secret store directly.
    pub fn list(&self) -> Value {
        json!(self
            .records()
            .into_iter()
            .map(|r| self.view(r))
            .collect::<Vec<Value>>())
    }

    fn view(&self, mut r: Value) -> Value {
        let id = r["id"].as_str().unwrap_or("").to_string();
        if let Some(m) = r.as_object_mut() {
            m.remove("secretRef");
            m.insert("hasSecret".into(), json!(self.secrets.has(&secret_ref(&id))));
        }
        r
    }

    /// Create or replace a record. Never carries a secret: a value arriving here
    /// is dropped rather than stored, because `integrations:setSecret` is the
    /// one path that writes one and it is the path that fails closed.
    pub fn upsert(&self, record: &Value) -> Value {
        let Some(id) = record["id"].as_str().filter(|s| !s.is_empty()) else {
            return json!({ "ok": false, "error": "an integration needs an id" });
        };
        if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return json!({ "ok": false, "error": "id must be alphanumeric, - or _" });
        }
        let auth = record["authType"].as_str().unwrap_or("none").to_string();

        let mut clean = record.clone();
        if let Some(m) = clean.as_object_mut() {
            // Defensive: a client that sends a secret in the record is making a
            // mistake, and silently persisting it into config.json would put a
            // credential in an unencrypted file.
            m.remove("secret");
            m.remove("token");
            m.remove("apiKey");
            m.insert("secretRef".into(), json!(secret_ref(id)));
        }

        let mut records = self.records();
        match records.iter().position(|r| r["id"].as_str() == Some(id)) {
            Some(i) => records[i] = clean.clone(),
            None => records.push(clean.clone()),
        }
        if let Err(e) = self.save_records(records) {
            return json!({ "ok": false, "error": e.to_string() });
        }
        let _ = auth;
        json!({ "ok": true, "record": self.view(clean) })
    }

    pub fn remove(&self, id: &str) -> Value {
        let records: Vec<Value> = self
            .records()
            .into_iter()
            .filter(|r| r["id"].as_str() != Some(id))
            .collect();
        if self.save_records(records).is_err() {
            return json!({ "ok": false });
        }
        // Remove the credential too. Leaving it behind means a later record
        // reusing the id silently inherits a secret nobody remembers setting.
        let _ = self.secrets.remove(&secret_ref(id));
        json!({ "ok": true })
    }

    /// Store a credential. The response says whether it worked and nothing else.
    pub fn set_secret(&self, id: &str, secret: &str) -> Value {
        if self.records().iter().all(|r| r["id"].as_str() != Some(id)) {
            return json!({ "ok": false, "error": "unknown integration" });
        }
        if secret.is_empty() {
            // Clearing is explicit, so an empty box does not read as "no change".
            return match self.secrets.remove(&secret_ref(id)) {
                Ok(_) => json!({ "ok": true }),
                Err(e) => json!({ "ok": false, "error": e.to_string() }),
            };
        }
        match self.secrets.set(&secret_ref(id), secret) {
            Ok(()) => json!({ "ok": true }),
            Err(e) => json!({ "ok": false, "error": e.to_string() }),
        }
    }

    /// Whether a record is usable right now: enabled, and holding a credential
    /// if its auth type needs one.
    pub fn enabled_ids(&self) -> Vec<String> {
        self.records()
            .iter()
            .filter(|r| r["enabled"].as_bool().unwrap_or(false))
            .filter(|r| {
                let auth = r["authType"].as_str().unwrap_or("none");
                !needs_secret(auth) || self.secrets.has(&secret_ref(r["id"].as_str().unwrap_or("")))
            })
            .filter_map(|r| r["id"].as_str().map(String::from))
            .collect()
    }

    /// Build the request an integration test should make.
    ///
    /// Returned rather than performed so the credential is applied in one place
    /// and the caller does the IO — and so this is testable without a network.
    pub fn test_request(&self, id: &str, path: Option<&str>) -> Result<(String, Vec<(String, String)>), String> {
        let records = self.records();
        let rec = records
            .iter()
            .find(|r| r["id"].as_str() == Some(id))
            .ok_or_else(|| "unknown integration".to_string())?;

        let base = rec["baseUrl"].as_str().unwrap_or("").trim_end_matches('/').to_string();
        if base.is_empty() {
            return Err("no base URL configured".into());
        }
        // Only https leaves this server. A credential attached to a plaintext
        // request is a credential on the wire.
        if !base.starts_with("https://") {
            return Err("base URL must be https".into());
        }
        let path = path.unwrap_or("/");
        let url = format!("{base}{}", if path.starts_with('/') { path.to_string() } else { format!("/{path}") });

        let auth = rec["authType"].as_str().unwrap_or("none");
        let mut headers = vec![("accept".to_string(), "application/json".to_string())];
        if needs_secret(auth) {
            let value = self
                .secrets
                .get(&secret_ref(id))
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "no secret stored for this integration".to_string())?;
            let header = match auth {
                "bearer" => ("authorization".to_string(), format!("Bearer {value}")),
                "basic" => ("authorization".to_string(), format!("Basic {value}")),
                _ => (
                    rec["headerName"].as_str().unwrap_or("authorization").to_lowercase(),
                    value,
                ),
            };
            headers.push(header);
        }
        Ok((url, headers))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::env_guard;

    fn store() -> (Integrations, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "md-int-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (Integrations::new(&dir), dir)
    }

    fn rec(id: &str, auth: &str) -> Value {
        json!({ "id": id, "name": id, "baseUrl": "https://api.example.com",
                "authType": auth, "enabled": true })
    }

    /// The contract: a client writes a secret and thereafter sees only whether
    /// one exists.
    #[test]
    fn a_listed_record_reports_only_whether_it_has_a_secret() {
        let _g = env_guard();
        let (i, _d) = store();
        i.upsert(&rec("svc", "bearer"));

        let list = i.list();
        assert_eq!(list[0]["hasSecret"], false);
        assert!(list[0].get("secretRef").is_none(), "the handle is internal");

        std::env::set_var("MD_SECRET_KEY", "k".repeat(40));
        i.set_secret("svc", "super-secret");
        assert_eq!(i.list()[0]["hasSecret"], true);
        // Still nowhere in the response.
        assert!(!i.list().to_string().contains("super-secret"));
        std::env::remove_var("MD_SECRET_KEY");
    }

    /// A client that puts a credential in the record is making a mistake;
    /// persisting it would write plaintext into config.json.
    #[test]
    fn a_secret_sent_in_a_record_is_dropped_not_persisted() {
        let (i, dir) = store();
        let mut r = rec("svc", "bearer");
        r["secret"] = json!("leaked-value");
        r["apiKey"] = json!("also-leaked");
        i.upsert(&r);

        let raw = std::fs::read_to_string(dir.join("config.json")).unwrap();
        assert!(!raw.contains("leaked-value"));
        assert!(!raw.contains("also-leaked"));
    }

    #[test]
    fn upsert_replaces_rather_than_duplicating_and_validates_the_id() {
        let (i, _d) = store();
        i.upsert(&rec("svc", "none"));
        i.upsert(&json!({ "id": "svc", "name": "renamed", "authType": "none" }));
        assert_eq!(i.list().as_array().unwrap().len(), 1);
        assert_eq!(i.list()[0]["name"], "renamed");

        assert_eq!(i.upsert(&json!({ "name": "no id" }))["ok"], false);
        assert_eq!(i.upsert(&json!({ "id": "../etc", "authType": "none" }))["ok"], false);
    }

    /// A later record reusing an id must not inherit a credential nobody
    /// remembers setting.
    #[test]
    fn removing_an_integration_removes_its_credential() {
        let _g = env_guard();
        std::env::set_var("MD_SECRET_KEY", "m".repeat(40));
        let (i, _d) = store();
        i.upsert(&rec("svc", "bearer"));
        i.set_secret("svc", "value");
        i.remove("svc");

        i.upsert(&rec("svc", "bearer"));
        assert_eq!(i.list()[0]["hasSecret"], false, "a reused id starts clean");
        std::env::remove_var("MD_SECRET_KEY");
    }

    #[test]
    fn only_records_that_can_authenticate_count_as_enabled() {
        let _g = env_guard();
        std::env::set_var("MD_SECRET_KEY", "n".repeat(40));
        let (i, _d) = store();
        i.upsert(&rec("open", "none"));
        i.upsert(&rec("needs", "bearer"));
        i.upsert(&json!({ "id": "off", "authType": "none", "enabled": false }));

        assert_eq!(i.enabled_ids(), ["open"], "a bearer record with no secret is not usable");
        i.set_secret("needs", "value");
        let mut ids = i.enabled_ids();
        ids.sort();
        assert_eq!(ids, ["needs", "open"]);
        std::env::remove_var("MD_SECRET_KEY");
    }

    /// A credential attached to a plaintext request is a credential on the wire.
    #[test]
    fn a_test_request_refuses_plaintext_http() {
        let (i, _d) = store();
        i.upsert(&json!({ "id": "svc", "baseUrl": "http://api.example.com", "authType": "none" }));
        assert!(i.test_request("svc", None).unwrap_err().contains("https"));

        i.upsert(&json!({ "id": "empty", "baseUrl": "", "authType": "none" }));
        assert!(i.test_request("empty", None).unwrap_err().contains("base URL"));
        assert!(i.test_request("missing", None).is_err());
    }

    #[test]
    fn a_test_request_attaches_the_credential_by_auth_type() {
        let _g = env_guard();
        std::env::set_var("MD_SECRET_KEY", "p".repeat(40));
        let (i, _d) = store();
        i.upsert(&rec("b", "bearer"));
        i.set_secret("b", "tok");
        let (url, headers) = i.test_request("b", Some("me")).unwrap();
        assert_eq!(url, "https://api.example.com/me");
        assert!(headers.contains(&("authorization".into(), "Bearer tok".into())));

        i.upsert(&json!({ "id": "h", "baseUrl": "https://x.dev", "authType": "header",
                          "headerName": "X-Api-Key", "enabled": true }));
        i.set_secret("h", "raw");
        let (_, headers) = i.test_request("h", None).unwrap();
        assert!(headers.contains(&("x-api-key".into(), "raw".into())));

        // Auth configured but nothing stored: refuse rather than send an
        // unauthenticated request that will look like a permissions problem.
        i.upsert(&rec("none", "bearer"));
        assert!(i.test_request("none", None).unwrap_err().contains("no secret"));
        std::env::remove_var("MD_SECRET_KEY");
    }
}
