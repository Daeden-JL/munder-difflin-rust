//! The realtime voice layer's server side.
//!
//! The voice session runs in the BROWSER, against the provider directly — the
//! server's job is to mint a short-lived token so the account key never reaches
//! the client, and to hold the two pieces of state a voice session cannot keep
//! itself: proposed actions awaiting confirmation, and completions to report.
//!
//! Actions are proposed, then confirmed or cancelled. That gate is the point:
//! a voice model mishearing "delete the branch" must not be able to act on it,
//! so nothing here executes — `propose` records an intent and `resolve` reports
//! the decision to whoever asked.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};

/// Proposals held before the oldest is dropped. A session that never confirms
/// anything must not grow memory without limit.
const MAX_PENDING: usize = 32;

/// Completions retained for a drain. The client polls; if it never does, this
/// is the ceiling.
const MAX_COMPLETIONS: usize = 64;

#[derive(Default)]
struct State {
    live: bool,
    pending: Vec<Value>,
    completions: Vec<Value>,
}

#[derive(Default)]
pub struct Realtime {
    state: Mutex<State>,
    /// task id → whether it finished. Read by `wait_for`.
    done: Mutex<HashMap<String, Value>>,
}

impl Realtime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_live(&self, live: bool) {
        let mut s = self.state.lock().unwrap();
        s.live = live;
        // Leaving a session drops proposals nobody can confirm any more. Keeping
        // them would let a stale intent be confirmed by the NEXT session.
        if !live {
            s.pending.clear();
        }
    }

    pub fn is_live(&self) -> bool {
        self.state.lock().unwrap().live
    }

    /// Record an action the voice model wants to take. Returns the id the
    /// operator confirms or cancels by.
    pub fn propose(&self, mut action: Value) -> Value {
        let id = crate::webhooks::random_hex(8);
        if let Some(m) = action.as_object_mut() {
            m.insert("id".into(), json!(id));
            m.insert("proposedAt".into(), json!(crate::hive::iso_now()));
        }
        let mut s = self.state.lock().unwrap();
        if s.pending.len() >= MAX_PENDING {
            s.pending.remove(0);
        }
        s.pending.push(action.clone());
        json!({ "ok": true, "id": id, "action": action })
    }

    /// Confirm or cancel a proposal. Removing it is what makes the decision
    /// final — a second confirmation of the same id finds nothing.
    pub fn resolve(&self, id: &str, confirmed: bool) -> Value {
        let mut s = self.state.lock().unwrap();
        let Some(i) = s.pending.iter().position(|a| a["id"] == id) else {
            return json!({ "ok": false, "error": "no such pending action" });
        };
        let action = s.pending.remove(i);
        json!({ "ok": true, "confirmed": confirmed, "action": action })
    }

    /// Record that a task finished, for `wait_for` and the completion drain.
    pub fn complete(&self, task_id: &str, summary: Value) {
        self.done.lock().unwrap().insert(task_id.to_string(), summary.clone());
        let mut s = self.state.lock().unwrap();
        if s.completions.len() >= MAX_COMPLETIONS {
            s.completions.remove(0);
        }
        s.completions.push(json!({ "taskId": task_id, "summary": summary }));
    }

    /// Take the completions since the last drain. Draining CLEARS them, so two
    /// clients cannot both announce the same finish.
    pub fn drain_completions(&self) -> Value {
        json!(std::mem::take(&mut self.state.lock().unwrap().completions))
    }

    /// Wait for a task to finish, up to `timeout_ms`.
    ///
    /// Polling rather than a condvar: the completion can arrive from any of
    /// several places (a hook, the router, an agent) and a poll keeps them from
    /// all having to know about this.
    pub async fn wait_for(&self, task_id: &str, timeout_ms: u64) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
        loop {
            if let Some(v) = self.done.lock().unwrap().get(task_id) {
                return json!({ "ok": true, "done": true, "summary": v });
            }
            if std::time::Instant::now() >= deadline {
                // Timing out is not failure: the task may still be running, and
                // saying so lets the caller decide whether to keep waiting.
                return json!({ "ok": true, "done": false, "timedOut": true });
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A voice model mishearing an instruction must not be able to act on it —
    /// nothing executes until an operator confirms.
    #[test]
    fn an_action_is_proposed_then_resolved_exactly_once() {
        let r = Realtime::new();
        let out = r.propose(json!({ "kind": "deleteBranch", "branch": "main" }));
        let id = out["id"].as_str().unwrap().to_string();
        assert_eq!(out["action"]["kind"], "deleteBranch");

        let ok = r.resolve(&id, true);
        assert_eq!(ok["confirmed"], true);
        assert_eq!(ok["action"]["branch"], "main");

        // A second decision on the same id finds nothing: the first is final.
        assert_eq!(r.resolve(&id, true)["ok"], false);
    }

    #[test]
    fn cancelling_is_recorded_as_a_decision_not_an_error() {
        let r = Realtime::new();
        let id = r.propose(json!({ "kind": "x" }))["id"].as_str().unwrap().to_string();
        let out = r.resolve(&id, false);
        assert_eq!(out["ok"], true);
        assert_eq!(out["confirmed"], false);
    }

    /// A stale intent must not be confirmable by the NEXT session.
    #[test]
    fn leaving_a_session_drops_unconfirmed_proposals() {
        let r = Realtime::new();
        let id = r.propose(json!({ "kind": "x" }))["id"].as_str().unwrap().to_string();
        r.set_live(true);
        r.set_live(false);
        assert_eq!(r.resolve(&id, true)["ok"], false);
        assert!(!r.is_live());
    }

    #[test]
    fn pending_actions_are_bounded() {
        let r = Realtime::new();
        for i in 0..MAX_PENDING + 10 {
            r.propose(json!({ "n": i }));
        }
        assert_eq!(r.state.lock().unwrap().pending.len(), MAX_PENDING);
    }

    /// Two clients must not both announce the same finish.
    #[test]
    fn draining_completions_clears_them() {
        let r = Realtime::new();
        r.complete("t1", json!("done"));
        assert_eq!(r.drain_completions().as_array().unwrap().len(), 1);
        assert_eq!(r.drain_completions().as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn waiting_returns_the_summary_or_reports_a_timeout() {
        let r = Realtime::new();
        r.complete("t1", json!("finished"));
        let out = r.wait_for("t1", 1_000).await;
        assert_eq!(out["done"], true);
        assert_eq!(out["summary"], "finished");

        // A timeout is not failure: the task may still be running.
        let out = r.wait_for("never", 300).await;
        assert_eq!(out["ok"], true);
        assert_eq!(out["done"], false);
        assert_eq!(out["timedOut"], true);
    }
}
