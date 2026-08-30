//! Closing time — the graceful, data-loss-free shutdown of a tenant's floor.
//!
//! Killing PTYs mid-thought loses whatever the agents held in working memory:
//! uncommitted work, unrecorded decisions, half-written `memory.md` files. This
//! closes the floor the way an office does — the human announces it, every
//! worker packs up and confirms, the manager locks the door.
//!
//! **The web version winds down a TENANT, never the process.** In Electron the
//! protocol ended in `app.quit()`; here that would take every other tenant's
//! floor down with it. Conclusion kills this tenant's PTY sessions and nothing
//! else. It follows that closing a browser tab must not start this: the tenant's
//! agents keep running until someone deliberately closes the floor.
//!
//! Everything rides the existing hive rails — inbox delivery for idle agents,
//! steer notes for busy ones. This module injects the kickoff mail and watches
//! routed traffic; it never types into a terminal.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::{json, Value};

/// How long before surfacing "this is taking a while". Compaction or a long
/// tool call can hold an ACK for minutes, so this is not a failure — it is the
/// point where the human deserves the option to stop waiting.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6 * 60);

/// Grace after COMPLETE, so the god's final commit and log writes land and the
/// floor visibly concludes rather than vanishing mid-sentence.
const TEARDOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(2_500);

/// Agents hand-write these subjects, so matching is forgiving about case and
/// separators: "Closing Time Ack" has to count as well as the canonical form
/// the brief asks for.
fn is_ack(subject: &str) -> bool {
    marker(subject, "ACK")
}
fn is_complete(subject: &str) -> bool {
    marker(subject, "COMPLETE")
}
fn marker(subject: &str, tail: &str) -> bool {
    let squashed: String = subject
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_uppercase();
    squashed.contains(&format!("CLOSINGTIME{tail}"))
}

#[derive(Default)]
struct State {
    active: bool,
    god: String,
    workers: BTreeSet<String>,
    acked: BTreeSet<String>,
    /// Bumped on every start/cancel/conclude so a timer from a previous run
    /// cannot fire into the current one.
    generation: u64,
}

pub struct Closing {
    state: Mutex<State>,
    generation: AtomicU64,
}

impl Default for Closing {
    fn default() -> Self {
        Self::new()
    }
}

/// What the caller must do in response. Returned rather than performed, so this
/// module stays testable without a hive, a pty manager or a socket.
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    /// Send this message to the god, from the human.
    Tell(Value),
    /// Every worker has confirmed. Kill this tenant's ptys after the grace.
    Conclude,
}

/// One queued steer note: which agent, and what to tell it.
#[derive(Debug)]
pub struct Steer {
    pub agent: String,
    pub note: String,
}

/// What starting the protocol produced. A struct rather than a tuple because
/// three of these are easy to pass in the wrong order.
#[derive(Debug)]
pub struct Started {
    pub progress: Progress,
    pub action: Action,
    /// Delivered through the control registry, not the inbox — see `start`.
    pub steers: Vec<Steer>,
}

/// What cancelling produced.
#[derive(Debug)]
pub struct Cancelled {
    pub progress: Progress,
    pub action: Action,
    /// Agents whose undelivered steer notes must be dropped.
    pub clear: Vec<String>,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub phase: &'static str,
    pub acked: usize,
    pub total: usize,
}


impl Closing {
    pub fn new() -> Self {
        Self { state: Mutex::new(State::default()), generation: AtomicU64::new(0) }
    }

    pub fn is_active(&self) -> bool {
        self.state.lock().unwrap().active
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    fn progress(s: &State, phase: &'static str) -> Progress {
        Progress { phase, acked: s.acked.len(), total: s.workers.len() }
    }

    /// Begin the protocol.
    ///
    /// `live` is the agents with a PTY right now. The registry alone will not
    /// do: agents that died with a crash keep their record without ever being
    /// archived, so a registry-based roster waits forever on ghosts.
    pub fn start(&self, registry: &Value, live: &[String]) -> Result<Started, String> {
        let mut s = self.state.lock().unwrap();
        if s.active {
            // Re-pressed while running (usually from the timeout view): keep
            // waiting rather than restarting the protocol.
            return Ok(Started {
                progress: Self::progress(&s, "progress"),
                action: Action::None,
                steers: vec![],
            });
        }

        let god = registry.get("godId").and_then(|v| v.as_str()).unwrap_or("god").to_string();
        let agents = registry.get("agents").and_then(|a| a.as_object());
        let live: BTreeSet<&String> = live.iter().collect();
        if agents.map(|a| !a.contains_key(&god)).unwrap_or(true) || !live.contains(&god) {
            return Err(
                "No orchestrator is running — closing time needs the god agent to collect the reports."
                    .into(),
            );
        }

        // Only agents with a live terminal are waited on. The registry supplies
        // names and the god flag here; it is never the roster source.
        s.workers = live
            .iter()
            .filter(|id| {
                ***id != god
                    && agents
                        .and_then(|a| a.get(**id))
                        .map(|a| !a.get("isGod").and_then(|v| v.as_bool()).unwrap_or(false))
                        .unwrap_or(false)
            })
            .map(|id| (*id).clone())
            .collect();
        s.acked.clear();
        s.god = god.clone();
        s.active = true;
        s.generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;

        let names = if s.workers.is_empty() {
            "(none — the floor is just you)".to_string()
        } else {
            s.workers
                .iter()
                .map(|id| {
                    let name = agents
                        .and_then(|a| a.get(id))
                        .and_then(|a| a.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(id);
                    format!("{name} ({id})")
                })
                .collect::<Vec<_>>()
                .join(", ")
        };

        let brief = json!({
            "to": "god",
            "act": "request",
            "subject": "CLOSING TIME — run the shutdown protocol now",
            "body": closing_brief(&names, s.workers.is_empty()),
        });

        // Steer notes are the graceful interrupt. The inbox brief only lands
        // when an agent next STOPS, so a worker hours into a task would hold the
        // whole shutdown; a steer rides its next hook boundary instead. Idle
        // agents are covered by the inbox, busy ones by the steer — both rails,
        // no typing into a terminal.
        let mut steers = vec![Steer { agent: god.clone(), note: GOD_STEER.to_string() }];
        for id in &s.workers {
            steers.push(Steer { agent: id.clone(), note: WORKER_STEER.to_string() });
        }

        Ok(Started { progress: Self::progress(&s, "started"), action: Action::Tell(brief), steers })
    }

    /// The human changed their mind. Returns the agents whose undelivered steer
    /// notes must be dropped, so a busy agent is not told to shut down after the
    /// cancel.
    pub fn cancel(&self) -> Option<Cancelled> {
        let mut s = self.state.lock().unwrap();
        if !s.active {
            return None;
        }
        s.active = false;
        s.generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        let mut clear: Vec<String> = s.workers.iter().cloned().collect();
        clear.push(s.god.clone());

        let tell = json!({
            "to": "god",
            "act": "inform",
            "subject": "CLOSING TIME CANCELLED",
            "body": "The human cancelled the shutdown — disregard the closing-time protocol \
                     and resume normal operation. Any memory saves already done are a bonus, \
                     not a problem.",
        });
        Some(Cancelled { progress: Self::progress(&s, "cancelled"), action: Action::Tell(tell), clear })
    }

    /// Observe one routed message.
    ///
    /// `delivered` is who actually took it, not who it was aimed at — an ACK
    /// that never reached the god has not happened.
    pub fn on_routed(
        &self,
        msg: &Value,
        delivered: &[String],
        registry: &Value,
        live: &[String],
    ) -> Option<(Progress, Action)> {
        let mut s = self.state.lock().unwrap();
        if !s.active {
            return None;
        }
        let subject = msg.get("subject").and_then(|v| v.as_str()).unwrap_or("");
        let from = msg.get("from").and_then(|v| v.as_str()).unwrap_or("");

        if is_ack(subject) && s.workers.contains(from) && delivered.contains(&s.god) {
            return s.acked.insert(from.to_string())
                .then(|| (Self::progress(&s, "progress"), Action::None));
        }

        if !is_complete(subject) || from != s.god {
            // COMPLETE is honoured only from the god: a worker must not be able
            // to close the whole floor, by accident or otherwise.
            return None;
        }

        // Trust but verify. The god is told to wait for every ACK, and the whole
        // point of closing time is that no worker loses unsaved state — so a
        // premature COMPLETE must not close the floor. Workers whose terminal
        // died mid-protocol are excused: their ACK can never arrive and their
        // session is gone either way.
        let live: BTreeSet<&String> = live.iter().collect();
        let archived = |id: &String| {
            registry
                .get("agents")
                .and_then(|a| a.get(id))
                .and_then(|a| a.get("archived"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        };
        let pending: Vec<String> = s
            .workers
            .iter()
            .filter(|id| !s.acked.contains(*id) && live.contains(id) && !archived(id))
            .cloned()
            .collect();

        if !pending.is_empty() {
            let names = pending
                .iter()
                .map(|id| {
                    let name = registry
                        .get("agents")
                        .and_then(|a| a.get(id))
                        .and_then(|a| a.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(id);
                    format!("{name} ({id})")
                })
                .collect::<Vec<_>>()
                .join(", ");
            let refuse = json!({
                "to": "god",
                "act": "refuse",
                "subject": "CLOSING TIME — conclusion rejected, workers still missing",
                "body": format!(
                    "The harness is still missing a CLOSING-TIME-ACK from: {names}.\n\
                     The floor stays open until every worker has confirmed its memory is saved.\n\
                     Chase the stragglers, wait for their ACKs, then send CLOSING-TIME-COMPLETE again."
                ),
            });
            return Some((Self::progress(&s, "progress"), Action::Tell(refuse)));
        }

        // Stays active through the grace so the UI holds the concluding state
        // instead of flicking back to normal for two seconds.
        s.generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        Some((Self::progress(&s, "complete"), Action::Conclude))
    }

    /// Called after the teardown grace. Returns false if the run was cancelled
    /// or superseded meanwhile, in which case the caller must NOT tear down.
    pub fn finish(&self, generation: u64) -> bool {
        let mut s = self.state.lock().unwrap();
        if s.generation != generation || !s.active {
            return false;
        }
        s.active = false;
        true
    }

    /// Whether a timeout for `generation` still describes the current run.
    pub fn timed_out(&self, generation: u64) -> Option<Progress> {
        let s = self.state.lock().unwrap();
        (s.active && s.generation == generation).then(|| Self::progress(&s, "timeout"))
    }

    pub fn teardown_grace() -> std::time::Duration {
        TEARDOWN_GRACE
    }
    pub fn timeout() -> std::time::Duration {
        TIMEOUT
    }
}

const GOD_STEER: &str = "CLOSING TIME was pressed by the human: pause your current work at the \
    next sensible point and drain your inbox NOW — a shutdown brief is waiting there. Coordinate \
    the floor shutdown before anything else.";

const WORKER_STEER: &str = "CLOSING TIME — the office is shutting down. Finish your current step \
    but do NOT start new work. Park or commit your work-in-progress safely, append your current \
    state + concrete next steps to your memory.md, then reply to god with a message whose subject \
    is exactly \"CLOSING-TIME-ACK\".";

fn closing_brief(names: &str, no_workers: bool) -> String {
    let tail = if no_workers {
        "There are no workers on the floor right now — do steps 3 and 4 immediately."
    } else {
        "The prep assistant saves its own memory separately — do NOT wait for it and do not message it."
    };
    format!(
        "The human pressed \"closing time\": the floor closes as soon as you confirm it is safe. \
         Run this protocol now, before anything else:\n\n\
         1. BROADCAST closing time to the team (message with \"to\":\"broadcast\"). Current workers: {names}.\n\
         \x20  Tell each worker to immediately: park or commit any work-in-progress safely, append its \
         current state + concrete next steps to its memory.md, and then reply to you with a message \
         whose subject is exactly \"CLOSING-TIME-ACK\".\n\
         2. WAIT and keep draining your inbox until EVERY worker above has sent its CLOSING-TIME-ACK. \
         Nudge stragglers once if needed.\n\
         3. Save your own state: update board.md and append your shift summary to your memory.md.\n\
         4. CONCLUDE by sending a message with \"to\":\"human\" and the subject exactly \
         \"CLOSING-TIME-COMPLETE\" — the harness watches for it and closes the floor. Do not send it \
         before every worker has acked: the harness independently verifies the ACKs and will reject \
         a premature conclusion.\n\n\
         {tail}\n\
         This is a shutdown: do not start new work and do not accept new tasks."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> Value {
        json!({ "godId": "michael", "agents": {
            "michael": { "name": "Michael", "isGod": true },
            "jim": { "name": "Jim" },
            "dwight": { "name": "Dwight" },
        }})
    }
    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    fn msg(from: &str, subject: &str) -> Value {
        json!({ "from": from, "subject": subject })
    }

    /// Agents hand-write these subjects, so the match has to be forgiving —
    /// but not so forgiving that unrelated mail closes the floor.
    #[test]
    fn ack_and_complete_markers_are_forgiving_but_not_loose() {
        for s in ["CLOSING-TIME-ACK", "Closing Time Ack", "closing_time_ack", "re: CLOSING-TIME-ACK"] {
            assert!(is_ack(s), "{s}");
        }
        assert!(is_complete("CLOSING-TIME-COMPLETE"));
        assert!(!is_ack("CLOSING-TIME-COMPLETE"));
        assert!(!is_complete("CLOSING-TIME-ACK"));
        assert!(!is_ack("closing the ticket"));
        assert!(!is_complete("task complete"));
    }

    /// The registry keeps records for agents that died with a crash, so a
    /// registry-based roster would wait forever on an agent that cannot ACK.
    #[test]
    fn only_agents_with_a_live_terminal_are_waited_on() {
        let c = Closing::new();
        // dwight is in the registry but has no pty.
        let out = c.start(&registry(), &ids(&["michael", "jim"])).unwrap();
        assert_eq!(out.progress.total, 1);
        assert!(matches!(out.action, Action::Tell(_)));
        // The god is steered too — it has to run the protocol.
        assert_eq!(out.steers.len(), 2);
        assert!(out.steers.iter().any(|s| s.agent == "michael"));
    }

    #[test]
    fn closing_time_needs_a_live_orchestrator() {
        let c = Closing::new();
        let err = c.start(&registry(), &ids(&["jim"])).unwrap_err();
        assert!(err.contains("No orchestrator"), "{err}");
        assert!(!c.is_active(), "a refused start must not leave the floor closing");
    }

    #[test]
    fn acks_are_counted_once_and_only_when_they_reach_the_god() {
        let c = Closing::new();
        c.start(&registry(), &ids(&["michael", "jim", "dwight"])).unwrap();
        let reg = registry();
        let live = ids(&["michael", "jim", "dwight"]);

        // Aimed at the god but never delivered — not an ACK that happened.
        assert!(c.on_routed(&msg("jim", "CLOSING-TIME-ACK"), &[], &reg, &live).is_none());

        let (p, _) = c
            .on_routed(&msg("jim", "CLOSING-TIME-ACK"), &ids(&["michael"]), &reg, &live)
            .unwrap();
        assert_eq!((p.acked, p.total), (1, 2));
        // A duplicate must not double-count.
        assert!(c.on_routed(&msg("jim", "CLOSING-TIME-ACK"), &ids(&["michael"]), &reg, &live).is_none());
    }

    /// The whole point is that no worker loses unsaved state, so a god that
    /// concludes early is refused rather than obeyed.
    #[test]
    fn a_premature_conclusion_is_rejected_and_says_who_is_missing() {
        let c = Closing::new();
        c.start(&registry(), &ids(&["michael", "jim", "dwight"])).unwrap();
        let reg = registry();
        let live = ids(&["michael", "jim", "dwight"]);
        c.on_routed(&msg("jim", "CLOSING-TIME-ACK"), &ids(&["michael"]), &reg, &live);

        let (p, action) = c
            .on_routed(&msg("michael", "CLOSING-TIME-COMPLETE"), &ids(&["michael"]), &reg, &live)
            .unwrap();
        assert_eq!(p.phase, "progress");
        match action {
            Action::Tell(m) => {
                assert_eq!(m["act"], "refuse");
                assert!(m["body"].as_str().unwrap().contains("Dwight (dwight)"));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(c.is_active(), "the floor stays open");
    }

    #[test]
    fn conclusion_is_accepted_once_every_live_worker_has_acked() {
        let c = Closing::new();
        c.start(&registry(), &ids(&["michael", "jim", "dwight"])).unwrap();
        let reg = registry();
        let live = ids(&["michael", "jim", "dwight"]);
        for w in ["jim", "dwight"] {
            c.on_routed(&msg(w, "CLOSING-TIME-ACK"), &ids(&["michael"]), &reg, &live);
        }
        let (p, action) = c
            .on_routed(&msg("michael", "CLOSING-TIME-COMPLETE"), &ids(&["michael"]), &reg, &live)
            .unwrap();
        assert_eq!(p.phase, "complete");
        assert_eq!(action, Action::Conclude);
    }

    /// A worker whose terminal died mid-protocol can never ACK, and its session
    /// is gone anyway — waiting on it would wedge the shutdown forever.
    #[test]
    fn a_worker_whose_terminal_died_does_not_block_the_conclusion() {
        let c = Closing::new();
        c.start(&registry(), &ids(&["michael", "jim", "dwight"])).unwrap();
        let reg = registry();
        c.on_routed(&msg("jim", "CLOSING-TIME-ACK"), &ids(&["michael"]), &reg, &ids(&["michael", "jim", "dwight"]));

        // dwight's pty is gone by the time the god concludes.
        let (_, action) = c
            .on_routed(&msg("michael", "CLOSING-TIME-COMPLETE"), &ids(&["michael"]), &reg, &ids(&["michael", "jim"]))
            .unwrap();
        assert_eq!(action, Action::Conclude);
    }

    /// A worker must not be able to close the floor.
    #[test]
    fn only_the_god_can_conclude() {
        let c = Closing::new();
        c.start(&registry(), &ids(&["michael", "jim"])).unwrap();
        let reg = registry();
        let live = ids(&["michael", "jim"]);
        assert!(c
            .on_routed(&msg("jim", "CLOSING-TIME-COMPLETE"), &ids(&["michael"]), &reg, &live)
            .is_none());
        assert!(c.is_active());
    }

    /// A cancelled run must not be torn down by the timer its start armed.
    #[test]
    fn a_stale_timer_cannot_tear_down_a_cancelled_run() {
        let c = Closing::new();
        c.start(&registry(), &ids(&["michael", "jim"])).unwrap();
        let gen = c.generation();
        assert!(c.timed_out(gen).is_some());

        let out = c.cancel().unwrap();
        assert_eq!(out.progress.phase, "cancelled");
        assert!(matches!(out.action, Action::Tell(_)));
        assert!(out.clear.contains(&"jim".to_string()) && out.clear.contains(&"michael".to_string()));

        assert!(c.timed_out(gen).is_none(), "the old timeout must not fire");
        assert!(!c.finish(gen), "the old teardown must not run");
        assert!(c.cancel().is_none(), "cancelling twice is a no-op");
    }

    #[test]
    fn re_pressing_while_running_keeps_waiting() {
        let c = Closing::new();
        c.start(&registry(), &ids(&["michael", "jim"])).unwrap();
        c.on_routed(&msg("jim", "CLOSING-TIME-ACK"), &ids(&["michael"]), &registry(), &ids(&["michael", "jim"]));

        let out = c.start(&registry(), &ids(&["michael", "jim"])).unwrap();
        assert_eq!(out.progress.phase, "progress");
        assert_eq!(out.progress.acked, 1, "progress must not be reset");
        assert_eq!(out.action, Action::None, "no second brief");
        assert!(out.steers.is_empty());
    }

    #[test]
    fn an_empty_floor_can_still_close() {
        let c = Closing::new();
        let out = c.start(&registry(), &ids(&["michael"])).unwrap();
        assert_eq!(out.progress.total, 0);
        match out.action {
            Action::Tell(m) => assert!(m["body"].as_str().unwrap().contains("no workers")),
            other => panic!("{other:?}"),
        }
        let (_, a) = c
            .on_routed(&msg("michael", "CLOSING-TIME-COMPLETE"), &ids(&["michael"]), &registry(), &ids(&["michael"]))
            .unwrap();
        assert_eq!(a, Action::Conclude);
    }
}
