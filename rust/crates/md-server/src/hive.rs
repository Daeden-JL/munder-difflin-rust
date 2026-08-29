//! The hive: the on-disk multi-agent coordination layer.
//!
//! Lives at `<harnessHome>/hive/` — `registry.json` (who exists), `tasks.json`
//! (the card ledger), `board.md` (the shared blackboard), `log.jsonl` (an
//! append-only event log), and `agents/<id>/{memory.md,inbox/,outbox/}`.
//!
//! **Unmodelled fields are preserved, everywhere.** These files are hand-written
//! by the god agent, which appends whatever a card or an agent record needs —
//! `result`, `repo`, `scope`, `origin`, `commit` — none of which any UI knows
//! about. The Electron original learned this the hard way (see
//! `src/shared/taskLedger.ts`): a writer holding a partial model silently
//! deleted every field it did not know about, on every card, the moment a user
//! touched one. So reads and writes here go through `serde_json::Value` rather
//! than typed structs. Typing these records would reintroduce exactly that bug,
//! which is why the shapes below are documentation instead of `#[derive]`.
//!
//! Not ported yet, and deliberately: agent spawning/provisioning, the hook
//! socket server, and the git single-committer. `WRITE_LOCK` gives writers
//! within one process the serialization the committer relied on.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Map, Value};

/// A runaway message must not ping-pong between agents forever. Matches the
/// Electron cap; there is no human queue to fall back on.
const HOP_CAP: u64 = 12;

/// Providers with no inbox-drain path. Mail to one of these cannot be delivered
/// into `inbox/` — the Electron original hands it to a terminal work-order
/// instead, and bounces to the god when that fails. With no handoff channel
/// ported yet, the bounce IS the behaviour here: loud, never silent.
const NO_INBOX_PROVIDERS: [&str; 3] = ["kimi", "copilot", "custom"];

/// Serializes writers across every tenant.
///
/// The Electron hive is a git repo with a single committer, which gave writes a
/// natural order. Until that lands, this lock supplies the same guarantee for
/// the read-modify-write cycles below — without it two concurrent card edits
/// interleave and one is lost.
fn write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub struct Hive {
    root: PathBuf,
}

/// Every mutation reports the same shape the bridge promised: `{ok}` plus an
/// `error` the UI can show. Failures are values here, not `Err` — a rejected
/// rename is a normal outcome, not a transport fault.
fn fail(msg: impl Into<String>) -> Value {
    json!({ "ok": false, "error": msg.into() })
}

impl Hive {
    pub fn new(hive_root: impl Into<PathBuf>) -> Self {
        Self { root: hive_root.into() }
    }

    /// The hive is optional: a tenant that has never run an agent has no hive
    /// directory, and every read below answers with an empty value rather than
    /// an error. An empty floor is a state, not a failure.
    pub fn enabled(&self) -> bool {
        self.root.is_dir()
    }

    fn agent_dir(&self, id: &str) -> PathBuf {
        self.root.join("agents").join(id)
    }

    fn read_json(&self, path: &Path, fallback: Value) -> Value {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(fallback)
    }

    /// Write via a temp file in the same directory, then rename.
    ///
    /// The rename is what matters: an agent polls these files continuously, and
    /// a plain truncate-then-write exposes a window where a reader sees a
    /// half-written file and parses it as empty.
    fn write_json(&self, path: &Path, value: &Value) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
        std::fs::rename(&tmp, path)
    }

    /// Append one event to `log.jsonl`. Best-effort by design: losing a log line
    /// must never fail the operation that produced it.
    fn append_log(&self, event: Value) {
        use std::io::Write;
        let _ = std::fs::create_dir_all(&self.root);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("log.jsonl"))
        {
            let _ = writeln!(f, "{event}");
        }
    }

    // ── Reads ───────────────────────────────────────────────────────────────

    /// `{ godId, agents: { id: {...} } }` — hive identity, distinct from the UI
    /// floor roster in `roster.json`.
    pub fn registry(&self) -> Value {
        self.read_json(
            &self.root.join("registry.json"),
            json!({ "godId": Value::Null, "agents": {} }),
        )
    }

    pub fn board(&self) -> String {
        std::fs::read_to_string(self.root.join("board.md")).unwrap_or_default()
    }

    /// The raw ledger envelope, `{ tasks: [...] }`, passed through untouched.
    pub fn tasks(&self) -> Value {
        self.read_json(&self.root.join("tasks.json"), json!({ "tasks": [] }))
    }

    fn task_list(&self) -> Vec<Value> {
        self.tasks()
            .get("tasks")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default()
    }

    /// The last `n` log events. A line that will not parse is surfaced as
    /// `{raw}` rather than dropped — a corrupt line is evidence, and silently
    /// skipping it hides whatever wrote it.
    pub fn log_tail(&self, n: usize) -> Value {
        let Ok(text) = std::fs::read_to_string(self.root.join("log.jsonl")) else {
            return json!([]);
        };
        let lines: Vec<&str> = text.trim().lines().filter(|l| !l.is_empty()).collect();
        let start = lines.len().saturating_sub(n);
        json!(lines[start..]
            .iter()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|_| json!({ "raw": l })))
            .collect::<Vec<Value>>())
    }

    pub fn memory(&self, id: &str) -> String {
        std::fs::read_to_string(self.agent_dir(id).join("memory.md")).unwrap_or_default()
    }

    pub fn inbox(&self, id: &str) -> Value {
        self.list_messages(&self.agent_dir(id).join("inbox"))
    }

    pub fn outbox(&self, id: &str) -> Value {
        self.list_messages(&self.agent_dir(id).join("outbox"))
    }

    /// Messages sort by filename, which is `<iso-stamp>-<rand>.json` — so the
    /// order is chronological without reading or parsing any of them.
    fn list_messages(&self, dir: &Path) -> Value {
        let Ok(entries) = std::fs::read_dir(dir) else { return json!([]) };
        let mut names: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        names.sort();
        json!(names
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .filter_map(|t| serde_json::from_str::<Value>(&t).ok())
            .collect::<Vec<Value>>())
    }

    // ── Task ledger ─────────────────────────────────────────────────────────

    /// Persist the ledger, folding `incoming` over what is on disk.
    ///
    /// Membership comes from `incoming` — a card dropped from the list is
    /// deleted, which is how dismiss works. What survives the fold are fields on
    /// the matching on-disk card that `incoming` does not mention.
    fn write_tasks(&self, incoming: Vec<Value>) {
        let merged = merge_task_ledger(&self.task_list(), &incoming);
        let n = merged.len();
        if self
            .write_json(&self.root.join("tasks.json"), &json!({ "tasks": merged }))
            .is_ok()
        {
            self.append_log(json!({ "kind": "tasks", "count": n }));
        }
    }

    /// Append one card against the CURRENT on-disk ledger, not against a list
    /// the caller read earlier — another writer (webhook, Slack, the god) may
    /// have added work in between. Idempotent by id.
    pub fn add_task(&self, task: Value) -> Value {
        let _g = write_lock().lock().unwrap();
        let Some(id) = task.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
            return fail("task needs a non-empty string id");
        };
        let mut tasks = self.task_list();
        if tasks.iter().any(|t| t.get("id").and_then(|v| v.as_str()) == Some(id)) {
            return json!({ "ok": false, "error": "task already exists" });
        }
        tasks.push(task);
        self.write_tasks(tasks);
        json!({ "ok": true })
    }

    pub fn patch_task(&self, id: &str, patch: &Value) -> Value {
        let _g = write_lock().lock().unwrap();
        let mut tasks = self.task_list();
        let Some(slot) = tasks
            .iter_mut()
            .find(|t| t.get("id").and_then(|v| v.as_str()) == Some(id))
        else {
            return fail("unknown task");
        };
        let Some(fields) = patch.as_object() else {
            return fail("patch must be an object");
        };
        if let Some(card) = slot.as_object_mut() {
            for (k, v) in fields {
                // `id` is the merge key for every other writer; letting a patch
                // move it would orphan the card rather than rename it.
                if k != "id" {
                    card.insert(k.clone(), v.clone());
                }
            }
        }
        self.write_tasks(tasks);
        json!({ "ok": true })
    }

    pub fn delete_task(&self, id: &str) -> Value {
        let _g = write_lock().lock().unwrap();
        let tasks = self.task_list();
        let next: Vec<Value> = tasks
            .iter()
            .filter(|t| t.get("id").and_then(|v| v.as_str()) != Some(id))
            .cloned()
            .collect();
        if next.len() == tasks.len() {
            return fail("unknown task");
        }
        self.write_tasks(next);
        json!({ "ok": true })
    }

    // ── Registry mutations ──────────────────────────────────────────────────

    /// Read-modify-write one agent record, preserving every field the caller did
    /// not name — the same rule the task ledger follows, for the same reason.
    fn patch_agent<F>(&self, id: &str, log: Value, edit: F) -> Value
    where
        F: FnOnce(&mut Map<String, Value>) -> Result<bool, String>,
    {
        let _g = write_lock().lock().unwrap();
        let mut reg = self.registry();
        let Some(agent) = reg
            .get_mut("agents")
            .and_then(|a| a.get_mut(id))
            .and_then(|a| a.as_object_mut())
        else {
            return fail("Agent not found");
        };
        match edit(agent) {
            Err(e) => fail(e),
            // The edit was a no-op — report success without touching the file,
            // so a redundant call does not churn the log or the mtime.
            Ok(false) => json!({ "ok": true }),
            Ok(true) => {
                if let Err(e) = self.write_json(&self.root.join("registry.json"), &reg) {
                    return fail(format!("could not write registry: {e}"));
                }
                self.append_log(log);
                json!({ "ok": true })
            }
        }
    }

    pub fn rename_agent(&self, id: &str, name: &str) -> Value {
        let next = name.trim().to_string();
        if next.is_empty() {
            return fail("Name is required");
        }
        // The registry key, agent directory, session id and every mailbox path
        // stay keyed by `id`; only the human-facing name moves.
        let res = self.patch_agent(id, json!({ "kind": "rename", "id": id, "name": next }), |a| {
            if a.get("name").and_then(|v| v.as_str()) == Some(next.as_str()) {
                return Ok(false);
            }
            a.insert("name".into(), json!(next));
            Ok(true)
        });
        match res.get("ok").and_then(|v| v.as_bool()) {
            Some(true) => json!({ "ok": true, "name": name.trim() }),
            _ => res,
        }
    }

    pub fn patch_agent_role(&self, id: &str, role: &str) -> Value {
        let next = role.trim().to_string();
        if next.is_empty() {
            return fail("empty role");
        }
        self.patch_agent(id, json!({ "kind": "role", "agentId": id, "role": next }), |a| {
            if a.get("role").and_then(|v| v.as_str()) == Some(next.as_str()) {
                return Ok(false);
            }
            a.insert("role".into(), json!(next));
            a.insert("lastSeen".into(), json!(now_ms()));
            Ok(true)
        })
    }

    pub fn set_archived(&self, id: &str, archived: bool) -> Value {
        self.patch_agent(
            id,
            json!({ "kind": "archive", "agentId": id, "archived": archived }),
            |a| {
                if a.get("archived").and_then(|v| v.as_bool()).unwrap_or(false) == archived {
                    return Ok(false);
                }
                a.insert("archived".into(), json!(archived));
                a.insert("lastSeen".into(), json!(now_ms()));
                Ok(true)
            },
        )
    }

    /// "Do not dispatch to them", not "they are gone" — a held agent keeps its
    /// terminal and stays active, which is why this is its own flag rather than
    /// a reuse of `archived`.
    pub fn set_agent_hold(&self, id: &str, hold: bool) -> Value {
        let res = self.patch_agent(id, json!({ "kind": "agent-hold", "id": id, "onHold": hold }), |a| {
            if a.get("onHold").and_then(|v| v.as_bool()).unwrap_or(false) == hold {
                return Ok(false);
            }
            a.insert("onHold".into(), json!(hold));
            Ok(true)
        });
        match res.get("ok").and_then(|v| v.as_bool()) {
            Some(true) => json!({ "ok": true, "onHold": hold }),
            _ => res,
        }
    }

    /// Remember the CLI session id a hook reported, for an idempotent
    /// `--resume` and for cost dedup. Writes only when it changes: a hook fires
    /// on every tool call, and rewriting the registry each time would churn the
    /// file agents poll.
    pub fn record_session(&self, id: &str, session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        self.patch_agent(id, json!({ "kind": "session", "agentId": id }), |a| {
            if a.get("sessionId").and_then(|v| v.as_str()) == Some(session_id) {
                return Ok(false);
            }
            a.insert("sessionId".into(), json!(session_id));
            a.insert("lastSeen".into(), json!(now_ms()));
            Ok(true)
        });
    }

    // ── Messaging ───────────────────────────────────────────────────────────

    /// Inject a message and route it. Returns the normalized message so the
    /// caller sees the id and defaults that were filled in.
    pub fn send(&self, partial: &Value, from: &str) -> Value {
        let _g = write_lock().lock().unwrap();
        let msg = normalize(partial, from);
        let delivered = self.route(&msg);
        json!({ "ok": true, "message": msg, "delivered": delivered })
    }

    /// Resolve targets and drop a copy in each one's inbox.
    ///
    /// Returns who actually took delivery, not who was aimed at — the log then
    /// reports delivery rather than intent, so a bounced message can never read
    /// as delivered.
    fn route(&self, msg: &Value) -> Vec<String> {
        let hops = msg.get("hops").and_then(|v| v.as_u64()).unwrap_or(0);
        let from = msg.get("from").and_then(|v| v.as_str()).unwrap_or("system").to_string();
        let to = msg.get("to").and_then(|v| v.as_str()).unwrap_or("god").to_string();
        let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();

        if hops > HOP_CAP {
            self.append_log(json!({ "kind": "drop", "reason": "hop-cap", "from": from, "to": to, "id": id }));
            return vec![];
        }

        let reg = self.registry();
        let empty = Map::new();
        let agents = reg.get("agents").and_then(|a| a.as_object()).unwrap_or(&empty);
        let god = reg
            .get("godId")
            .and_then(|v| v.as_str())
            .unwrap_or("god")
            .to_string();

        let flag = |id: &str, key: &str| {
            agents.get(id).and_then(|a| a.get(key)).and_then(|v| v.as_bool()).unwrap_or(false)
        };
        let provider = |id: &str| {
            agents
                .get(id)
                .and_then(|a| a.get("provider"))
                .and_then(|v| v.as_str())
                .unwrap_or("claude")
                .to_string()
        };

        let targets: Vec<String> = if to == "broadcast" {
            // Fan-out is the ACTIVE registry: never the sender, never the
            // send-only prep assistant, never an archived agent. Hookless
            // providers are NOT excluded — the per-target path below decides how
            // each one is served, and excluding them here once made a broadcast
            // invisible to an agent that direct mail reached fine.
            let mut ids: Vec<String> = agents
                .keys()
                .filter(|id| **id != from && !flag(id, "isAssistant") && !flag(id, "archived"))
                .cloned()
                .collect();
            ids.sort();
            ids
        } else {
            // "human" and "god" both resolve to the orchestrator: the hive keeps
            // no separate human queue, because approvals are native to each
            // agent's own session. Never deliver to self, which would loop a
            // god → "human" message straight back to god.
            let resolved = if to == "human" || to == "god" { god.clone() } else { to.clone() };
            if resolved == from { vec![] } else { vec![resolved] }
        };

        let mut delivered = Vec::new();
        for t in targets {
            // The prep assistant is send-only and drains no inbox, so direct
            // mail to it would rot unread. Bounce to god instead: the sender's
            // intent surfaces immediately rather than being lost quietly.
            if flag(&t, "isAssistant") {
                self.bounce(msg, &god, &format!(
                    "[bounced — \"{t}\" is the send-only prep assistant; route work to a real agent]"
                ));
                continue;
            }
            // A provider with no inbox-drain path gets a terminal work-order in
            // the Electron original. That channel is not ported, so this takes
            // the original's own fallback: bounce to god to relay.
            if t != god && NO_INBOX_PROVIDERS.contains(&provider(&t).as_str()) {
                self.bounce(msg, &god, &format!(
                    "[undeliverable — \"{}\" runs {} and has no inbox drain; relay this to it]",
                    t,
                    provider(&t)
                ));
                continue;
            }
            if self.deliver(msg, &t) {
                delivered.push(t);
            } else {
                self.append_log(json!({ "kind": "drop", "reason": "no-inbox", "to": t, "id": id }));
            }
        }
        self.append_log(json!({ "kind": "msg", "from": from, "to": to, "id": id, "delivered": delivered }));
        delivered
    }

    fn bounce(&self, msg: &Value, god: &str, prefix: &str) {
        let mut bounced = msg.clone();
        if let Some(m) = bounced.as_object_mut() {
            let subject = msg.get("subject").and_then(|v| v.as_str()).unwrap_or("");
            m.insert("to".into(), json!(god));
            m.insert("subject".into(), json!(format!("{prefix} {subject}")));
        }
        self.deliver(&bounced, god);
    }

    /// An absent inbox means an unknown recipient; the caller logs the drop
    /// rather than letting the message vanish.
    fn deliver(&self, msg: &Value, to: &str) -> bool {
        let inbox = self.agent_dir(to).join("inbox");
        if !inbox.is_dir() {
            return false;
        }
        let id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("message");
        self.write_json(&inbox.join(format!("{id}.json")), msg).is_ok()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `2026-08-29T12-34-56-789Z` — an ISO stamp with `:` and `.` replaced, because
/// this becomes a filename and the sort order of those filenames IS the
/// chronological order `list_messages` relies on.
fn stamp() -> String {
    let ms = now_ms();
    let (secs, milli) = (ms / 1000, ms % 1000);
    let days = secs / 86_400;
    let tod = secs % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm), shifted to a 0000-03-01 era.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}-{:02}-{:02}-{milli:03}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Six hex characters of entropy, enough to separate two messages stamped in
/// the same millisecond.
fn short_rand() -> String {
    use rand::Rng;
    format!("{:06x}", rand::thread_rng().gen::<u32>() & 0xff_ffff)
}

/// Fill in the fields a caller left out. Unknown keys on `partial` are carried
/// through untouched — same rule as the ledger.
fn normalize(partial: &Value, from: &str) -> Value {
    let mut m = partial.as_object().cloned().unwrap_or_default();
    let act = m.get("act").and_then(|v| v.as_str()).unwrap_or("inform").to_string();

    let mut put = |k: &str, v: Value| {
        if !m.contains_key(k) || m[k].is_null() {
            m.insert(k.to_string(), v);
        }
    };
    put("id", json!(format!("{}-{}", stamp(), short_rand())));
    put("conversation", json!(format!("conv-{}", short_rand())));
    put("from", json!(from));
    put("to", json!("god"));
    put("act", json!(act.clone()));
    put("subject", json!(""));
    put("body", json!(""));
    put("hops", json!(0));
    // These three acts expect an answer, so the default follows the act rather
    // than being a blanket false.
    put("requires_reply", json!(matches!(act.as_str(), "request" | "query" | "propose")));
    put("needs_human", json!(false));
    put("created_at", json!(iso_now()));
    // `in_reply_to` is explicitly nullable: absent and null mean the same thing,
    // so it cannot go through `put`, which treats null as absent.
    m.entry("in_reply_to").or_insert(Value::Null);
    Value::Object(m)
}

fn iso_now() -> String {
    let s = stamp();
    // Undo the filename-safe substitutions for the timestamp field itself.
    let (date, time) = s.split_once('T').unwrap_or((&s, ""));
    let t: Vec<&str> = time.trim_end_matches('Z').split('-').collect();
    if t.len() == 4 {
        format!("{date}T{}:{}:{}.{}Z", t[0], t[1], t[2], t[3])
    } else {
        s
    }
}

/// Fold `incoming` over `existing`, matching cards by `id`.
///
/// The result is `incoming` — its order and its membership, so removing a card
/// still deletes it. What survives are the fields on the matching on-disk card
/// that `incoming` does not mention.
///
/// A field the caller DOES send wins, including an explicit `null`: that is the
/// way to clear one. A missing key means "I don't know about this", never
/// "remove it" — writers are partial models. Cards with no string `id` pass
/// through untouched, because there is no key to merge them on and dropping
/// them would lose data the same way this function exists to prevent.
pub fn merge_task_ledger(existing: &[Value], incoming: &[Value]) -> Vec<Value> {
    let mut by_id: HashMap<&str, &Map<String, Value>> = HashMap::new();
    for e in existing {
        if let (Some(obj), Some(id)) = (e.as_object(), e.get("id").and_then(|v| v.as_str())) {
            if !id.is_empty() {
                by_id.entry(id).or_insert(obj);
            }
        }
    }
    incoming
        .iter()
        .map(|card| {
            let Some(obj) = card.as_object() else { return card.clone() };
            let Some(id) = obj.get("id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) else {
                return card.clone();
            };
            let Some(old) = by_id.get(id) else { return card.clone() };
            let mut merged = (*old).clone();
            for (k, v) in obj {
                merged.insert(k.clone(), v.clone());
            }
            Value::Object(merged)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let p = std::env::temp_dir().join(format!("md-hive-{}", short_rand()));
        std::fs::create_dir_all(p.join("agents")).unwrap();
        p
    }

    /// The bug this whole module is shaped around: a UI writing back a card it
    /// only partly models must not delete the god's own fields.
    #[test]
    fn merging_keeps_fields_the_writer_does_not_model() {
        let existing = vec![json!({
            "id": "t1", "title": "old", "status": "todo",
            "result": "the verbatim Slack reply", "repo": "acme/web",
            "webhook": { "tokenHash": "abc" }
        })];
        let incoming = vec![json!({ "id": "t1", "title": "new", "status": "done" })];
        let merged = merge_task_ledger(&existing, &incoming);

        assert_eq!(merged[0]["title"], "new", "a named field must win");
        assert_eq!(merged[0]["result"], "the verbatim Slack reply");
        assert_eq!(merged[0]["webhook"]["tokenHash"], "abc");
    }

    #[test]
    fn merging_protects_fields_never_membership() {
        let existing = vec![json!({ "id": "a" }), json!({ "id": "b" })];
        let merged = merge_task_ledger(&existing, &[json!({ "id": "a" })]);
        assert_eq!(merged.len(), 1, "a card dropped from the list is deleted");
    }

    /// An explicit null is how a caller clears a field; treating it as "absent"
    /// would make clearing impossible.
    #[test]
    fn an_explicit_null_clears_a_field() {
        let existing = vec![json!({ "id": "t1", "assignee": "dwight" })];
        let merged = merge_task_ledger(&existing, &[json!({ "id": "t1", "assignee": null })]);
        assert!(merged[0]["assignee"].is_null());
    }

    #[test]
    fn cards_without_an_id_pass_through_rather_than_vanish() {
        let merged = merge_task_ledger(&[], &[json!({ "title": "hand-written, no id" })]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["title"], "hand-written, no id");
    }

    #[test]
    fn patching_an_agent_preserves_unmodelled_fields() {
        let h = Hive::new(tmp());
        h.write_json(
            &h.root.join("registry.json"),
            &json!({ "godId": "michael", "agents": {
                "dwight": { "name": "Dwight", "cwd": "/w", "sessionId": "s-1", "customField": 7 }
            }}),
        )
        .unwrap();

        assert_eq!(h.rename_agent("dwight", "  Dwight K.  ")["name"], "Dwight K.");
        let a = &h.registry()["agents"]["dwight"];
        assert_eq!(a["name"], "Dwight K.");
        assert_eq!(a["sessionId"], "s-1", "the resume key must survive a rename");
        assert_eq!(a["customField"], 7);

        assert_eq!(h.rename_agent("nobody", "X")["error"], "Agent not found");
        assert_eq!(h.rename_agent("dwight", "   ")["error"], "Name is required");
    }

    /// Hold and archive are different states and must not collapse into one.
    #[test]
    fn hold_and_archive_are_independent_flags() {
        let h = Hive::new(tmp());
        h.write_json(
            &h.root.join("registry.json"),
            &json!({ "godId": "michael", "agents": { "jim": { "name": "Jim" } } }),
        )
        .unwrap();

        assert_eq!(h.set_agent_hold("jim", true)["onHold"], true);
        assert_eq!(h.set_archived("jim", true)["ok"], true);
        let a = &h.registry()["agents"]["jim"];
        assert_eq!(a["onHold"], true);
        assert_eq!(a["archived"], true);
    }

    #[test]
    fn tasks_round_trip_through_add_patch_delete() {
        let h = Hive::new(tmp());
        assert_eq!(h.add_task(json!({ "id": "t1", "title": "one", "extra": "keep me" }))["ok"], true);
        assert_eq!(h.add_task(json!({ "id": "t1", "title": "dup" }))["ok"], false);
        assert_eq!(h.add_task(json!({ "title": "no id" }))["ok"], false);

        assert_eq!(h.patch_task("t1", &json!({ "status": "done" }))["ok"], true);
        let t = &h.tasks()["tasks"][0];
        assert_eq!(t["status"], "done");
        assert_eq!(t["extra"], "keep me", "a patch must not drop unmodelled fields");
        assert_eq!(t["title"], "one");

        // Moving the merge key would orphan the card rather than rename it.
        h.patch_task("t1", &json!({ "id": "t2" }));
        assert_eq!(h.tasks()["tasks"][0]["id"], "t1");

        assert_eq!(h.delete_task("t1")["ok"], true);
        assert_eq!(h.delete_task("t1")["ok"], false);
    }

    /// A hook fires on every tool call, so an unchanged session id must not
    /// rewrite the registry — agents poll that file.
    #[test]
    fn recording_a_session_id_is_idempotent() {
        let h = Hive::new(tmp());
        h.write_json(
            &h.root.join("registry.json"),
            &json!({ "godId": "michael", "agents": { "jim": { "name": "Jim" } } }),
        )
        .unwrap();

        h.record_session("jim", "sess-1");
        assert_eq!(h.registry()["agents"]["jim"]["sessionId"], "sess-1");

        let before = std::fs::metadata(h.root.join("registry.json")).unwrap().modified().unwrap();
        h.record_session("jim", "sess-1");
        let after = std::fs::metadata(h.root.join("registry.json")).unwrap().modified().unwrap();
        assert_eq!(before, after, "an unchanged id must not rewrite the file");

        h.record_session("jim", "sess-2");
        assert_eq!(h.registry()["agents"]["jim"]["sessionId"], "sess-2");
    }

    #[test]
    fn a_message_reaches_the_recipient_inbox() {
        let h = Hive::new(tmp());
        std::fs::create_dir_all(h.agent_dir("jim").join("inbox")).unwrap();
        h.write_json(
            &h.root.join("registry.json"),
            &json!({ "godId": "michael", "agents": { "jim": {}, "michael": {} } }),
        )
        .unwrap();

        let sent = h.send(&json!({ "to": "jim", "subject": "hi", "act": "request" }), "human");
        assert_eq!(sent["delivered"][0], "jim");
        // `request` expects an answer, so the default follows the act.
        assert_eq!(sent["message"]["requires_reply"], true);
        assert!(sent["message"]["in_reply_to"].is_null());

        let inbox = h.inbox("jim");
        assert_eq!(inbox.as_array().unwrap().len(), 1);
        assert_eq!(inbox[0]["subject"], "hi");
        assert_eq!(inbox[0]["from"], "human");
    }

    /// "human" is not a mailbox — it resolves to the god, who is the human's
    /// proxy on the floor.
    #[test]
    fn mail_to_human_lands_with_the_god() {
        let h = Hive::new(tmp());
        std::fs::create_dir_all(h.agent_dir("michael").join("inbox")).unwrap();
        h.write_json(
            &h.root.join("registry.json"),
            &json!({ "godId": "michael", "agents": { "michael": {} } }),
        )
        .unwrap();

        assert_eq!(h.send(&json!({ "to": "human" }), "jim")["delivered"][0], "michael");
        // …but the god messaging "human" must not loop back to itself.
        assert_eq!(
            h.send(&json!({ "to": "human" }), "michael")["delivered"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn broadcast_skips_the_sender_the_assistant_and_archived_agents() {
        let h = Hive::new(tmp());
        for id in ["michael", "jim", "pam", "creed"] {
            std::fs::create_dir_all(h.agent_dir(id).join("inbox")).unwrap();
        }
        h.write_json(
            &h.root.join("registry.json"),
            &json!({ "godId": "michael", "agents": {
                "michael": {}, "jim": {}, "pam": { "isAssistant": true }, "creed": { "archived": true }
            }}),
        )
        .unwrap();

        let out = h.send(&json!({ "to": "broadcast" }), "michael");
        assert_eq!(out["delivered"], json!(["jim"]));
    }

    #[test]
    fn a_runaway_message_is_dropped_at_the_hop_cap() {
        let h = Hive::new(tmp());
        std::fs::create_dir_all(h.agent_dir("jim").join("inbox")).unwrap();
        h.write_json(
            &h.root.join("registry.json"),
            &json!({ "godId": "michael", "agents": { "jim": {} } }),
        )
        .unwrap();

        let out = h.send(&json!({ "to": "jim", "hops": HOP_CAP + 1 }), "human");
        assert_eq!(out["delivered"].as_array().unwrap().len(), 0);
        assert_eq!(h.inbox("jim").as_array().unwrap().len(), 0);
    }

    /// Mail to a provider that cannot drain an inbox must surface loudly at the
    /// god, never rot unread in a mailbox nothing reads.
    #[test]
    fn mail_to_a_hookless_provider_bounces_to_the_god() {
        let h = Hive::new(tmp());
        for id in ["michael", "ryan"] {
            std::fs::create_dir_all(h.agent_dir(id).join("inbox")).unwrap();
        }
        h.write_json(
            &h.root.join("registry.json"),
            &json!({ "godId": "michael", "agents": {
                "michael": {}, "ryan": { "provider": "custom" }
            }}),
        )
        .unwrap();

        h.send(&json!({ "to": "ryan", "subject": "do the thing" }), "human");
        assert_eq!(h.inbox("ryan").as_array().unwrap().len(), 0);
        let god = h.inbox("michael");
        assert_eq!(god.as_array().unwrap().len(), 1);
        assert!(god[0]["subject"].as_str().unwrap().starts_with("[undeliverable"));
    }

    #[test]
    fn a_corrupt_log_line_is_surfaced_not_skipped() {
        let h = Hive::new(tmp());
        std::fs::write(h.root.join("log.jsonl"), "{\"kind\":\"a\"}\nnot json\n").unwrap();
        let tail = h.log_tail(10);
        assert_eq!(tail[0]["kind"], "a");
        assert_eq!(tail[1]["raw"], "not json");
    }

    #[test]
    fn an_absent_hive_reads_as_empty_rather_than_failing() {
        let h = Hive::new(std::env::temp_dir().join("md-hive-does-not-exist"));
        assert!(!h.enabled());
        assert_eq!(h.registry()["agents"], json!({}));
        assert_eq!(h.tasks()["tasks"], json!([]));
        assert_eq!(h.board(), "");
        assert_eq!(h.log_tail(10), json!([]));
        assert_eq!(h.inbox("nobody"), json!([]));
    }

    /// Filenames carry the sort order the inbox listing depends on, so the
    /// stamp must be lexicographically sortable and free of `:` and `.`.
    #[test]
    fn stamps_sort_chronologically_and_are_filename_safe() {
        let s = stamp();
        assert!(!s.contains(':') && !s.contains('.'), "{s} is not filename-safe");
        assert_eq!(s.len(), 24, "{s}");
        assert!(s.starts_with("20"), "{s}");
        assert!(iso_now().contains(':'), "the timestamp field keeps real ISO form");
    }
}
