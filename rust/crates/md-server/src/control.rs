//! Operator control over running agents.
//!
//! The floor exerts control WITHOUT typing into the PTY: every decision here is
//! read by the hook server and returned on Claude Code's own hook-return
//! protocol. That distinction is the whole design —
//!
//!   * `pause` / `gate_tool` → `PreToolUse` answers `permissionDecision: deny`,
//!     decided immediately with no round-trip, so it cannot hit the shim's
//!     timeout. Slow *human* approval deliberately still rides Claude's native
//!     permission prompt; this is the fast, mechanical refusal.
//!   * `steer` → the next `UserPromptSubmit`/`PostToolUse` returns
//!     `additionalContext`, injecting guidance once.
//!   * `halt` → the next hook boundary returns `continue: false`, stopping the
//!     agent *cleanly* instead of killing its PTY — the session id stays valid
//!     for a later `--resume`.
//!
//! State is per tenant and in memory: it describes agents that are running now,
//! so it is meaningless across a restart that took those agents with it.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Mutex;

use serde::Serialize;

/// Steer notes queued per agent before the OLDEST is dropped.
///
/// Each note rides the next hook's `additionalContext`, so a halted or stalled
/// agent never drains its queue — uncapped, a burst of steers grows forever.
/// When full, drop from the FRONT: the newest instruction is the one still
/// worth delivering.
const MAX_PENDING_STEERS: usize = 20;

/// Matches the hook `additionalContext` cap.
const MAX_STEER_LEN: usize = 10_000;

#[derive(Debug, Default, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub paused: bool,
    pub halted: bool,
    pub auto_delivery_paused: bool,
    /// Sorted, so a snapshot is stable to compare and to render.
    pub gated_tools: Vec<String>,
    pub pending_steers: usize,
}

#[derive(Default)]
struct Agent {
    paused: bool,
    halted: bool,
    auto_delivery_paused: bool,
    gated_tools: BTreeSet<String>,
    steers: VecDeque<String>,
}

impl Agent {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            paused: self.paused,
            halted: self.halted,
            auto_delivery_paused: self.auto_delivery_paused,
            gated_tools: self.gated_tools.iter().cloned().collect(),
            pending_steers: self.steers.len(),
        }
    }
}

#[derive(Default)]
pub struct Control {
    agents: Mutex<HashMap<String, Agent>>,
}

pub struct ToolDecision {
    pub deny: bool,
    pub reason: Option<String>,
}

impl Control {
    pub fn new() -> Self {
        Self::default()
    }

    fn edit<T>(&self, id: &str, f: impl FnOnce(&mut Agent) -> T) -> T {
        let mut g = self.agents.lock().unwrap();
        f(g.entry(id.to_string()).or_default())
    }

    // ── Operator actions ────────────────────────────────────────────────────

    pub fn pause(&self, id: &str, on: bool) -> Snapshot {
        self.edit(id, |a| {
            a.paused = on;
            a.snapshot()
        })
    }

    /// Clears pause AND halt so a stopped agent can run again. Tool gates
    /// survive: they are a standing policy, not part of the stop.
    pub fn resume(&self, id: &str) -> Snapshot {
        self.edit(id, |a| {
            a.paused = false;
            a.halted = false;
            a.snapshot()
        })
    }

    pub fn halt(&self, id: &str) -> Snapshot {
        self.edit(id, |a| {
            a.halted = true;
            a.snapshot()
        })
    }

    pub fn set_auto_delivery_paused(&self, id: &str, paused: bool) -> Snapshot {
        self.edit(id, |a| {
            a.auto_delivery_paused = paused;
            a.snapshot()
        })
    }

    pub fn gate_tool(&self, id: &str, tool: &str, on: bool) -> Snapshot {
        self.edit(id, |a| {
            if on {
                a.gated_tools.insert(tool.to_string());
            } else {
                a.gated_tools.remove(tool);
            }
            a.snapshot()
        })
    }

    /// Queue one steer note. Empty text is not an instruction and is dropped
    /// rather than delivered as a blank context injection.
    pub fn steer(&self, id: &str, text: &str) -> Snapshot {
        let t = text.trim();
        if t.is_empty() {
            return self.snapshot(id);
        }
        let mut note: String = t.chars().take(MAX_STEER_LEN).collect();
        note.shrink_to_fit();
        self.edit(id, |a| {
            if a.steers.len() >= MAX_PENDING_STEERS {
                // A full queue means the agent has been unreachable for a long
                // time; the dropped note is worth a breadcrumb.
                tracing::warn!(agent = id, "steer queue full — dropping oldest note");
                a.steers.pop_front();
            }
            a.steers.push_back(note);
            a.snapshot()
        })
    }

    /// Drop queued-but-undelivered notes — e.g. closing time cancelled before a
    /// busy agent's next hook boundary consumed the instruction.
    pub fn clear_steers(&self, id: &str) {
        self.edit(id, |a| a.steers.clear());
    }

    // ── Reads, used by the hook server ──────────────────────────────────────

    pub fn should_halt(&self, id: &str) -> bool {
        self.agents.lock().unwrap().get(id).is_some_and(|a| a.halted)
    }

    pub fn is_auto_delivery_paused(&self, id: &str) -> bool {
        self.agents.lock().unwrap().get(id).is_some_and(|a| a.auto_delivery_paused)
    }

    /// A paused agent denies every tool; otherwise only gated ones. The reason
    /// travels back to the model, so it is written for the model to act on.
    pub fn tool_decision(&self, id: &str, tool: &str) -> ToolDecision {
        let g = self.agents.lock().unwrap();
        let Some(a) = g.get(id) else {
            return ToolDecision { deny: false, reason: None };
        };
        if a.paused {
            return ToolDecision {
                deny: true,
                reason: Some("Paused by operator — resume from the floor to continue.".into()),
            };
        }
        if !tool.is_empty() && a.gated_tools.contains(tool) {
            return ToolDecision {
                deny: true,
                reason: Some(format!("Tool {tool} is gated by the operator.")),
            };
        }
        ToolDecision { deny: false, reason: None }
    }

    /// Dequeue one note for delivery. Taking it is what makes a steer
    /// deliver-once.
    pub fn take_steer(&self, id: &str) -> Option<String> {
        self.agents.lock().unwrap().get_mut(id)?.steers.pop_front()
    }

    /// An agent nobody has ever acted on reports the all-default snapshot rather
    /// than nothing — "no controls applied" is an answer, not a missing record.
    pub fn snapshot(&self, id: &str) -> Snapshot {
        self.agents
            .lock()
            .unwrap()
            .get(id)
            .map(Agent::snapshot)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paused_agent_denies_every_tool_a_gate_denies_only_its_own() {
        let c = Control::new();
        assert!(!c.tool_decision("jim", "Bash").deny, "unknown agent is unrestricted");

        c.gate_tool("jim", "Bash", true);
        assert!(c.tool_decision("jim", "Bash").deny);
        assert!(!c.tool_decision("jim", "Read").deny, "a gate is per tool");

        c.pause("jim", true);
        assert!(c.tool_decision("jim", "Read").deny, "pause is not per tool");
    }

    /// Resuming must not silently drop a standing tool gate — that would let a
    /// tool the operator forbade run again on an unrelated action.
    #[test]
    fn resume_clears_the_stop_but_keeps_gates() {
        let c = Control::new();
        c.pause("jim", true);
        c.halt("jim");
        c.gate_tool("jim", "Bash", true);

        let s = c.resume("jim");
        assert!(!s.paused && !s.halted);
        assert_eq!(s.gated_tools, vec!["Bash"]);
        assert!(c.tool_decision("jim", "Bash").deny);
    }

    #[test]
    fn a_steer_is_delivered_once() {
        let c = Control::new();
        c.steer("jim", "  check the stapler  ");
        assert_eq!(c.snapshot("jim").pending_steers, 1);
        assert_eq!(c.take_steer("jim").as_deref(), Some("check the stapler"));
        assert_eq!(c.take_steer("jim"), None);
    }

    #[test]
    fn an_empty_steer_is_not_an_instruction() {
        let c = Control::new();
        c.steer("jim", "   \n ");
        assert_eq!(c.snapshot("jim").pending_steers, 0);
    }

    /// The cap drops the OLDEST: a stalled agent's next boundary should hear the
    /// most recent instruction, not the stalest one.
    #[test]
    fn a_full_steer_queue_drops_the_oldest_note() {
        let c = Control::new();
        for i in 0..MAX_PENDING_STEERS + 5 {
            c.steer("jim", &format!("note {i}"));
        }
        assert_eq!(c.snapshot("jim").pending_steers, MAX_PENDING_STEERS);
        assert_eq!(c.take_steer("jim").as_deref(), Some("note 5"));
    }

    #[test]
    fn steer_notes_are_truncated_to_the_hook_context_cap() {
        let c = Control::new();
        c.steer("jim", &"x".repeat(MAX_STEER_LEN + 500));
        assert_eq!(c.take_steer("jim").unwrap().len(), MAX_STEER_LEN);
    }

    #[test]
    fn an_untouched_agent_reports_defaults() {
        let s = Control::new().snapshot("nobody");
        assert!(!s.paused && !s.halted && !s.auto_delivery_paused);
        assert!(s.gated_tools.is_empty());
        assert_eq!(s.pending_steers, 0);
    }
}
