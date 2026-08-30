//! The knowledge store — ingest documents, search them by keyword.
//!
//! File-backed, not a database, and deliberately so: the Electron original is a
//! directory an agent CLI reads out-of-process, so the layout IS the interface.
//! Changing it to sqlite would break every agent that reads the store directly.
//!
//! ```text
//! <root>/index.jsonl        one JSON line per CHUNK — the search index
//! <root>/docs/<docId>/meta.json
//! <root>/docs/<docId>/text.md
//! ```
//!
//! Scoring is keyword, not embeddings: log-damped term frequency, a title
//! boost, a breadth bonus for matching distinct terms, and an exact-phrase
//! bonus. Ported to match the original's ranking, because agents and the UI
//! search the same store and disagreeing about relevance would be worse than
//! either ranking alone.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Characters per chunk, broken on a boundary where one is available.
const CHUNK_SIZE: usize = 1_200;
/// Carried into the next chunk, so a passage straddling a boundary is still
/// findable from either side.
const CHUNK_OVERLAP: usize = 150;
/// Per-document text indexed before truncation.
const MAX_INDEX_CHARS: usize = 5 * 1024 * 1024;
const DEFAULT_SEARCH_LIMIT: usize = 8;
/// Context either side of a match in a snippet.
const SNIPPET_RADIUS: usize = 160;

const STOPWORDS: &str = "a an and are as at be but by for from has have how in is it its of on or \
that the their this to was were what when where which who will with your you we our us they them \
he she his her i me my do does did not can could would should about into over than then there \
here so if no yes";

pub struct Knowledge {
    root: PathBuf,
}

/// Words worth indexing: alphanumeric runs, longer than one character, that are
/// not stopwords. Matching the original's tokenizer matters more than improving
/// it — the index on disk was written by it.
fn tokenize(s: &str) -> Vec<String> {
    let stop: Vec<&str> = STOPWORDS.split(' ').collect();
    s.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() > 1 && !stop.contains(t))
        .map(String::from)
        .collect()
}

/// Split into chunks, preferring paragraph, line, then word boundaries.
///
/// Deterministic and always terminating: the cursor advances by at least one
/// character even when no boundary is found, which is what stops a pathological
/// input from looping forever.
fn chunk_text(text: &str) -> Vec<String> {
    let t: String = text.replace("\r\n", "\n").trim().to_string();
    if t.is_empty() {
        return vec![];
    }
    let chars: Vec<char> = t.chars().collect();
    if chars.len() <= CHUNK_SIZE {
        return vec![t];
    }

    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let mut end = (i + CHUNK_SIZE).min(chars.len());
        if end < chars.len() {
            let window: String = chars[i..end].iter().collect();
            let br = window
                .rfind("\n\n")
                .or_else(|| window.rfind('\n'))
                .or_else(|| window.rfind(' '));
            // Only break LATE. A boundary near the start would produce a run of
            // tiny chunks and bloat the index.
            if let Some(br) = br {
                let br_chars = window[..br].chars().count();
                if br_chars > CHUNK_SIZE * 6 / 10 {
                    end = i + br_chars;
                }
            }
        }
        let piece: String = chars[i..end].iter().collect::<String>().trim().to_string();
        if !piece.is_empty() {
            out.push(piece);
        }
        if end >= chars.len() {
            break;
        }
        i = (end.saturating_sub(CHUNK_OVERLAP)).max(i + 1);
    }
    out
}

/// Score one chunk. Zero means no query term appears, and the caller drops it.
fn score_chunk(rec: &Value, terms: &[String], raw_query: &str) -> f64 {
    if terms.is_empty() {
        return 0.0;
    }
    let text = rec.get("text").and_then(|v| v.as_str()).unwrap_or("");
    let title = rec.get("title").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();

    let mut tf: HashMap<String, u32> = HashMap::new();
    for tok in tokenize(text) {
        *tf.entry(tok).or_insert(0) += 1;
    }

    let mut score = 0.0;
    let mut matched = 0u32;
    for term in terms {
        let c = *tf.get(term).unwrap_or(&0);
        if c > 0 {
            matched += 1;
            // Log-damped: a term appearing fifty times is not fifty times as
            // relevant as one appearing once.
            score += 1.0 + (1.0 + c as f64).ln();
        }
        if title.contains(term) {
            score += 2.0;
        }
    }
    if matched == 0 {
        return 0.0;
    }
    // Breadth: a chunk covering more of the query beats one repeating a word.
    score += matched as f64 * 0.5;

    let q = raw_query.trim().to_lowercase();
    if q.len() >= 3 && text.to_lowercase().contains(&q) {
        score += 5.0;
    }
    score
}

fn make_snippet(text: &str, terms: &[String]) -> String {
    let t: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = t.chars().collect();
    let lc = t.to_lowercase();

    let at = terms
        .iter()
        .filter_map(|term| lc.find(term.as_str()))
        .min()
        .map(|b| lc[..b].chars().count());

    let Some(at) = at else {
        return if chars.len() > SNIPPET_RADIUS * 2 {
            chars[..SNIPPET_RADIUS * 2].iter().collect::<String>() + "…"
        } else {
            t
        };
    };

    let start = at.saturating_sub(SNIPPET_RADIUS);
    let end = (at + SNIPPET_RADIUS).min(chars.len());
    let body: String = chars[start..end].iter().collect::<String>().trim().to_string();
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        body,
        if end < chars.len() { "…" } else { "" }
    )
}

impl Knowledge {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn docs(&self) -> PathBuf {
        self.root.join("docs")
    }
    fn index(&self) -> PathBuf {
        self.root.join("index.jsonl")
    }

    /// Ingest text as one document. Returns its id and chunk count.
    ///
    /// The id is derived from the title and the clock rather than the content:
    /// re-ingesting an edited file is a NEW document, and the caller decides
    /// whether to remove the old one. Content-hashing would silently merge two
    /// revisions into one.
    pub fn ingest(&self, title: &str, source: &str, text: &str, tags: &[String]) -> Value {
        let title = if title.trim().is_empty() { "untitled" } else { title.trim() };
        let doc_id = format!("{}-{}", slug(title), now_ms());
        let doc_dir = self.docs().join(&doc_id);
        if let Err(e) = std::fs::create_dir_all(&doc_dir) {
            return json!({ "ok": false, "srcPath": source, "error": e.to_string() });
        }

        let truncated = text.chars().count() > MAX_INDEX_CHARS;
        let indexed: String = text.chars().take(MAX_INDEX_CHARS).collect();
        if std::fs::write(doc_dir.join("text.md"), text).is_err() {
            return json!({ "ok": false, "srcPath": source, "error": "could not write text" });
        }

        let mut chunks = chunk_text(&indexed);
        // An empty or image-only document still indexes its title, or it would
        // be in the store and findable by nothing.
        if chunks.is_empty() {
            chunks = vec![title.to_string()];
        }

        let lines: Vec<String> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| {
                json!({ "docId": doc_id, "title": title, "source": source,
                        "modality": "text", "chunkIdx": i, "text": c })
                .to_string()
            })
            .collect();

        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.index()) {
            let _ = writeln!(f, "{}", lines.join("\n"));
        }

        let meta = json!({
            "id": doc_id, "title": title, "source": source, "modality": "text",
            "mime": Value::Null, "origExt": ext_of(source), "bytes": text.len(),
            "tags": tags, "caption": Value::Null, "chunkCount": chunks.len(),
            "addedAt": iso_now(), "extractor": "text", "truncated": truncated,
        });
        let _ = std::fs::write(
            doc_dir.join("meta.json"),
            serde_json::to_vec_pretty(&meta).unwrap_or_default(),
        );

        json!({ "ok": true, "srcPath": source, "docId": doc_id, "chunkCount": chunks.len() })
    }

    pub fn search(&self, query: &str, limit: usize) -> Value {
        let terms = tokenize(query);
        if terms.is_empty() {
            return json!([]);
        }
        let limit = if limit == 0 { DEFAULT_SEARCH_LIMIT } else { limit.min(100) };
        let Ok(text) = std::fs::read_to_string(self.index()) else { return json!([]) };

        let mut scored: Vec<(f64, Value)> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|rec| {
                let s = score_chunk(&rec, &terms, query);
                (s > 0.0).then_some((s, rec))
            })
            .collect();

        // Ties break on docId then chunk index, so the same query always returns
        // the same order — an unstable sort here would shuffle results between
        // identical searches.
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1["docId"].as_str().cmp(&b.1["docId"].as_str()))
                .then_with(|| {
                    a.1["chunkIdx"].as_u64().cmp(&b.1["chunkIdx"].as_u64())
                })
        });

        json!(scored
            .into_iter()
            .take(limit)
            .map(|(score, rec)| json!({
                "docId": rec["docId"], "title": rec["title"], "source": rec["source"],
                "modality": rec["modality"], "chunkIdx": rec["chunkIdx"],
                "score": (score * 1000.0).round() / 1000.0,
                "snippet": make_snippet(rec["text"].as_str().unwrap_or(""), &terms),
            }))
            .collect::<Vec<Value>>())
    }

    /// Every document's metadata, newest first.
    pub fn list(&self) -> Value {
        let Ok(dir) = std::fs::read_dir(self.docs()) else { return json!([]) };
        let mut out: Vec<Value> = dir
            .filter_map(Result::ok)
            .filter_map(|e| std::fs::read_to_string(e.path().join("meta.json")).ok())
            .filter_map(|t| serde_json::from_str(&t).ok())
            .collect();
        out.sort_by(|a, b| {
            b["addedAt"].as_str().unwrap_or("").cmp(a["addedAt"].as_str().unwrap_or(""))
        });
        json!(out)
    }

    pub fn get(&self, doc_id: &str) -> Value {
        let Some(dir) = self.doc_dir(doc_id) else { return Value::Null };
        let Ok(meta) = std::fs::read_to_string(dir.join("meta.json")) else { return Value::Null };
        let Ok(meta) = serde_json::from_str::<Value>(&meta) else { return Value::Null };
        json!({ "meta": meta, "text": std::fs::read_to_string(dir.join("text.md")).unwrap_or_default() })
    }

    /// Remove a document and drop its lines from the index. Both, or the search
    /// index keeps returning hits for a document that no longer exists.
    pub fn remove(&self, doc_id: &str) -> Value {
        let Some(dir) = self.doc_dir(doc_id) else { return json!({ "ok": false }) };
        let _ = std::fs::remove_dir_all(&dir);

        if let Ok(text) = std::fs::read_to_string(self.index()) {
            let kept: Vec<&str> = text
                .lines()
                .filter(|l| {
                    serde_json::from_str::<Value>(l)
                        .map(|r| r["docId"].as_str() != Some(doc_id))
                        .unwrap_or(true)
                })
                .collect();
            let body = if kept.is_empty() { String::new() } else { kept.join("\n") + "\n" };
            let _ = std::fs::write(self.index(), body);
        }
        json!({ "ok": true })
    }

    pub fn status(&self) -> Value {
        let docs = self.list();
        let list = docs.as_array().cloned().unwrap_or_default();
        let chunk_count: u64 = list.iter().filter_map(|d| d["chunkCount"].as_u64()).sum();
        let mut by_modality = serde_json::Map::new();
        for d in &list {
            let m = d["modality"].as_str().unwrap_or("text").to_string();
            let n = by_modality.get(&m).and_then(|v| v.as_u64()).unwrap_or(0);
            by_modality.insert(m, json!(n + 1));
        }
        json!({
            "enabled": self.root.is_dir(),
            "root": self.root,
            "docCount": list.len(),
            "chunkCount": chunk_count,
            "byModality": by_modality,
        })
    }

    /// Resolve a document directory, refusing an id that could traverse.
    fn doc_dir(&self, doc_id: &str) -> Option<PathBuf> {
        if doc_id.is_empty()
            || !doc_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return None;
        }
        let p = self.docs().join(doc_id);
        p.is_dir().then_some(p)
    }
}

fn slug(s: &str) -> String {
    let out: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = out.trim_matches('-').to_string();
    let short: String = trimmed.chars().take(48).collect();
    if short.is_empty() { "doc".into() } else { short }
}

fn ext_of(source: &str) -> String {
    Path::new(source)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn iso_now() -> String {
    crate::hive::iso_now()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Knowledge {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "md-kg-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        Knowledge::new(root)
    }

    #[test]
    fn stopwords_and_single_characters_are_not_indexed() {
        assert_eq!(tokenize("The quick brown fox is a X"), ["quick", "brown", "fox"]);
        assert_eq!(tokenize("!!! ???"), Vec::<String>::new());
    }

    /// The overlap is what makes a passage straddling a boundary findable from
    /// either side.
    #[test]
    fn chunking_overlaps_and_always_terminates() {
        let short = chunk_text("one paragraph");
        assert_eq!(short, ["one paragraph"]);

        // No boundaries at all — the pathological case for a boundary-seeking
        // splitter.
        let solid = "x".repeat(CHUNK_SIZE * 3);
        let chunks = chunk_text(&solid);
        assert!(chunks.len() >= 3);
        assert!(chunks.iter().all(|c| c.chars().count() <= CHUNK_SIZE));

        let text = format!("{}\n\n{}", "a ".repeat(700), "b ".repeat(700));
        let chunks = chunk_text(&text);
        assert!(chunks.len() > 1);
    }

    #[test]
    fn search_ranks_a_title_match_and_an_exact_phrase_above_a_passing_mention() {
        let k = store();
        k.ingest("Stapler policy", "policy.md", "The stapler must stay on the desk.", &[]);
        k.ingest("Unrelated", "other.md", "A passing mention of a stapler somewhere.", &[]);

        let hits = k.search("stapler policy", 10);
        let hits = hits.as_array().unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["title"], "Stapler policy", "the title match ranks first");
        assert!(hits[0]["score"].as_f64().unwrap() > hits[1]["score"].as_f64().unwrap());
        assert!(hits[0]["snippet"].as_str().unwrap().contains("stapler"));
    }

    #[test]
    fn a_query_of_only_stopwords_finds_nothing() {
        let k = store();
        k.ingest("Doc", "d.md", "content here", &[]);
        assert!(k.search("the and of", 10).as_array().unwrap().is_empty());
        assert!(k.search("", 10).as_array().unwrap().is_empty());
    }

    /// Identical searches must return identical order, or results shuffle
    /// between two runs of the same query.
    #[test]
    fn ranking_is_stable_across_identical_searches() {
        let k = store();
        for i in 0..5 {
            k.ingest(&format!("Doc {i}"), "d.md", "the same body text about widgets", &[]);
        }
        let a = k.search("widgets", 10);
        let b = k.search("widgets", 10);
        assert_eq!(a, b);
    }

    /// Removing a document must drop its index lines too, or search keeps
    /// returning hits for something that no longer exists.
    #[test]
    fn removing_a_document_also_removes_it_from_the_index() {
        let k = store();
        let a = k.ingest("Keep", "a.md", "widgets are fine", &[]);
        let b = k.ingest("Drop", "b.md", "widgets are doomed", &[]);
        assert_eq!(k.search("widgets", 10).as_array().unwrap().len(), 2);

        assert_eq!(k.remove(b["docId"].as_str().unwrap())["ok"], true);
        let hits = k.search("widgets", 10);
        assert_eq!(hits.as_array().unwrap().len(), 1);
        assert_eq!(hits[0]["docId"], a["docId"]);
        assert!(k.get(b["docId"].as_str().unwrap()).is_null());
    }

    #[test]
    fn a_document_id_cannot_traverse() {
        let k = store();
        assert!(k.get("../../etc").is_null());
        assert_eq!(k.remove("../../etc")["ok"], false);
    }

    /// An image or an empty file still has to be findable by its title.
    #[test]
    fn an_empty_document_indexes_its_title() {
        let k = store();
        let r = k.ingest("Quarterly diagram", "q.png", "", &[]);
        assert_eq!(r["chunkCount"], 1);
        assert_eq!(k.search("quarterly", 10).as_array().unwrap().len(), 1);
    }

    #[test]
    fn status_counts_documents_and_chunks() {
        let k = store();
        k.ingest("A", "a.md", &"word ".repeat(2_000), &[]);
        k.ingest("B", "b.md", "short", &[]);
        let s = k.status();
        assert_eq!(s["docCount"], 2);
        assert!(s["chunkCount"].as_u64().unwrap() > 2, "the long doc chunked");
        assert_eq!(s["enabled"], true);
    }

    #[test]
    fn list_is_newest_first_and_get_round_trips_the_text() {
        let k = store();
        let a = k.ingest("First", "a.md", "body A", &["x".into()]);
        std::thread::sleep(std::time::Duration::from_millis(5));
        k.ingest("Second", "b.md", "body B", &[]);

        let list = k.list();
        assert_eq!(list[0]["title"], "Second");
        let got = k.get(a["docId"].as_str().unwrap());
        assert_eq!(got["text"], "body A");
        assert_eq!(got["meta"]["tags"][0], "x");
    }
}
