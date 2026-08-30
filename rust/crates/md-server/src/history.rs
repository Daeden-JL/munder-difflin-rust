//! Command history — what was typed at each agent, and when.
//!
//! An append-only JSONL file per tenant. The Electron original keeps this in
//! sqlite alongside other persisted state; a log is the right shape on its own
//! (append, read the tail, substring search) and avoids pulling a database in
//! for one table.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Entries kept before the file is trimmed. History is a convenience, not a
/// record: an unbounded file would grow for the life of the install.
const MAX_ENTRIES: usize = 5_000;

/// Longest single entry retained. A pasted file would otherwise sit in history
/// forever and swamp every search.
const MAX_TEXT: usize = 8_000;

/// Trimming reads and rewrites the whole file, so it is triggered by SIZE
/// rather than run on every add — otherwise appending is O(n) per call and the
/// file's own growth makes it quadratic.
const TRIM_ABOVE_BYTES: u64 = 4 * 1024 * 1024;

pub struct History {
    path: PathBuf,
}

impl History {
    pub fn new(harness_home: &Path) -> Self {
        Self { path: harness_home.join("history.jsonl") }
    }

    fn entries(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    pub fn add(&self, agent_id: &str, cwd: Option<&str>, text: &str) -> Value {
        if agent_id.is_empty() || text.trim().is_empty() {
            return json!({ "ok": false, "error": "invalid args" });
        }
        let trimmed: String = text.chars().take(MAX_TEXT).collect();
        let entry = json!({
            "id": now_ms(),
            "agentId": agent_id,
            "cwd": cwd,
            "text": trimmed,
            "ts": now_ms(),
        });

        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // Append first: the common path touches only the end of the file.
        use std::io::Write;
        match std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            Ok(mut f) => {
                let _ = writeln!(f, "{entry}");
            }
            Err(e) => return json!({ "ok": false, "error": e.to_string() }),
        }

        self.trim_if_large();
        json!({ "ok": true })
    }

    /// Rewrite the file down to `MAX_ENTRIES`, but only once it has grown past
    /// the byte threshold. Checking size is a stat; checking the entry count
    /// would mean parsing everything on every add.
    fn trim_if_large(&self) {
        let too_big = std::fs::metadata(&self.path).map(|m| m.len() > TRIM_ABOVE_BYTES).unwrap_or(false);
        if !too_big {
            return;
        }
        let all = self.entries();
        if all.len() <= MAX_ENTRIES {
            return;
        }
        let keep = &all[all.len() - MAX_ENTRIES..];
        let out: Vec<String> = keep.iter().map(|e| e.to_string()).collect();
        let _ = std::fs::write(&self.path, out.join("\n") + "\n");
    }

    /// Newest first — history is read from the most recent backwards.
    pub fn list(&self, agent_id: Option<&str>, limit: usize) -> Value {
        let mut all = self.entries();
        all.reverse();
        json!(all
            .into_iter()
            .filter(|e| agent_id.is_none_or(|id| e["agentId"] == id))
            .take(limit)
            .collect::<Vec<Value>>())
    }

    pub fn search(&self, query: &str, limit: usize) -> Value {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return json!([]);
        }
        let mut all = self.entries();
        all.reverse();
        json!(all
            .into_iter()
            .filter(|e| e["text"].as_str().is_some_and(|t| t.to_lowercase().contains(&q)))
            .take(limit)
            .collect::<Vec<Value>>())
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir()
            .join(format!("md-hist-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn history_reads_newest_first_and_filters_by_agent() {
        let h = History::new(&tmp());
        h.add("jim", Some("/w"), "first");
        h.add("pam", None, "other agent");
        h.add("jim", Some("/w"), "second");

        let jim = h.list(Some("jim"), 10);
        let texts: Vec<&str> = jim.as_array().unwrap().iter()
            .map(|e| e["text"].as_str().unwrap()).collect();
        assert_eq!(texts, ["second", "first"], "newest first");
        assert_eq!(h.list(None, 10).as_array().unwrap().len(), 3);
        assert_eq!(h.list(Some("jim"), 1).as_array().unwrap().len(), 1);
    }

    #[test]
    fn search_is_case_insensitive_and_needs_a_query() {
        let h = History::new(&tmp());
        h.add("jim", None, "Refactor the AUTH module");
        assert_eq!(h.search("auth", 10).as_array().unwrap().len(), 1);
        assert_eq!(h.search("   ", 10).as_array().unwrap().len(), 0);
        assert_eq!(h.search("nothing", 10).as_array().unwrap().len(), 0);
    }

    #[test]
    fn empty_and_oversized_entries_are_rejected_or_trimmed() {
        let h = History::new(&tmp());
        assert_eq!(h.add("jim", None, "   ")["ok"], false);
        assert_eq!(h.add("", None, "text")["ok"], false);

        h.add("jim", None, &"x".repeat(MAX_TEXT + 500));
        let len = h.list(None, 1)[0]["text"].as_str().unwrap().chars().count();
        assert_eq!(len, MAX_TEXT, "a pasted file must not live in history forever");
    }

    /// History is a convenience, not a record — the file has to self-limit.
    /// Driven by writing a large file directly rather than by looping `add`
    /// thousands of times: the point is the trim, not the append.
    #[test]
    fn the_file_is_trimmed_once_it_grows_past_the_threshold() {
        let dir = tmp();
        let h = History::new(&dir);
        let filler = "y".repeat(1_200);
        let lines: Vec<String> = (0..MAX_ENTRIES + 500)
            .map(|i| json!({ "id": i, "agentId": "jim", "text": format!("{filler} {i}"), "ts": i })
                .to_string())
            .collect();
        std::fs::write(dir.join("history.jsonl"), lines.join("\n") + "\n").unwrap();
        assert!(std::fs::metadata(dir.join("history.jsonl")).unwrap().len() > TRIM_ABOVE_BYTES);

        h.add("jim", None, "the newest one");
        assert_eq!(h.entries().len(), MAX_ENTRIES);
        assert_eq!(h.list(None, 1)[0]["text"], "the newest one", "the tail is what survives");
    }
}
