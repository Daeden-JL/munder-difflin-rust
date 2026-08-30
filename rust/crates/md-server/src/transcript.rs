//! Reading an agent's session transcript.
//!
//! This is what the conversation view is built from. The Electron app inferred
//! agent activity by screen-scraping the TUI (`usePtyParser.ts`, whose own
//! header calls that "a stopgap until we wire real Claude Code hooks"); the
//! transcript is the structured record that scraping approximated, so the web
//! client reads real tool names and arguments instead of regexed glyphs.
//!
//! The file is JSONL written by the CLI, one record per line, appended as the
//! session runs. Reading is therefore **incremental**: a caller passes back the
//! byte offset it last saw and gets only what arrived since.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

/// Cap on a single read, so following a long-running agent cannot return
/// megabytes in one response. The cursor makes the next call cheap.
const MAX_READ_BYTES: u64 = 512 * 1024;

/// Tool output can be enormous (a file read, a long build log). The view shows
/// a summary, so the rest is dropped at the source rather than pushed to a
/// client that would only truncate it anyway.
const MAX_RESULT_CHARS: usize = 2_000;

/// Claude Code's project key: the absolute cwd with every non-alphanumeric
/// character turned into a dash — leading slash and dots included.
/// `/Users/me/app` → `-Users-me-app`.
pub fn project_key(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Where the CLI keeps transcripts for a working directory.
///
/// `home` is the TENANT's home, not the server user's: each tenant's agents run
/// with `HOME` pointed at their own directory, so this must follow.
pub fn project_dir(home: &Path, cwd: &str) -> PathBuf {
    home.join(".claude/projects").join(project_key(cwd))
}

/// The transcript for one session, if the CLI has written it yet.
pub fn session_file(home: &Path, cwd: &str, session_id: &str) -> Option<PathBuf> {
    // A session id reaches the filesystem as a path segment, so anything that
    // could traverse is refused rather than sanitised.
    if session_id.is_empty()
        || !session_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let p = project_dir(home, cwd).join(format!("{session_id}.jsonl"));
    p.is_file().then_some(p)
}

/// One entry in the conversation view.
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// `prompt` | `say` | `think` | `tool` | `result`
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Tool name, for `tool` entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The tool's headline argument — a path, a command — which is what makes a
    /// tool line readable at a glance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arg: Option<String>,
    /// `result` entries only: whether the tool reported failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<String>,
    /// True for a subagent's traffic, so the view can fold it away.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub sidechain: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub entries: Vec<Entry>,
    /// Byte offset to pass back next time. Always on a line boundary, so a
    /// record still being written is simply re-read once complete.
    pub cursor: u64,
    /// True when the cap stopped this read short — call again immediately
    /// rather than waiting for new output.
    pub more: bool,
}

/// Read from `cursor` to the end of the file (or the cap).
///
/// A cursor beyond the file's length means the transcript was replaced — a new
/// session writing to the same path — so it restarts from zero rather than
/// returning nothing forever.
pub fn read(path: &Path, cursor: u64) -> std::io::Result<Page> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = if cursor > len { 0 } else { cursor };
    let want = (len - start).min(MAX_READ_BYTES);

    let mut buf = vec![0u8; want as usize];
    f.seek(SeekFrom::Start(start))?;
    f.read_exact(&mut buf)?;

    // Stop at the last newline: a trailing partial record is left for the next
    // call, when the writer has finished it.
    let end = buf.iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
    let text = String::from_utf8_lossy(&buf[..end]);

    let mut entries = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(rec) = serde_json::from_str::<Value>(line) {
            push_entry(&rec, &mut entries);
        }
    }

    Ok(Page {
        entries,
        cursor: start + end as u64,
        more: start + want < len,
    })
}

/// Turn one record into zero or more view entries.
///
/// Most record types are not conversation: the CLI also writes titles, mode
/// changes, queue operations and file snapshots into the same file. Only the
/// two that carry a message are read, and meta records among those are skipped
/// — they are machinery the user never typed and the model never said.
fn push_entry(rec: &Value, out: &mut Vec<Entry>) {
    let ty = rec.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if !matches!(ty, "user" | "assistant") {
        return;
    }
    let flag = |k: &str| rec.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    if flag("isMeta") || flag("isVisibleInTranscriptOnly") {
        return;
    }
    let sidechain = flag("isSidechain");
    let ts = rec.get("timestamp").and_then(|v| v.as_str()).map(String::from);
    let mk = |kind: &'static str| Entry {
        kind,
        text: None,
        tool: None,
        arg: None,
        error: None,
        ts: ts.clone(),
        sidechain,
    };

    let content = rec.get("message").and_then(|m| m.get("content"));

    // A bare string is the plain-prompt shape.
    if let Some(s) = content.and_then(|c| c.as_str()) {
        let t = s.trim();
        if !t.is_empty() && !is_command_wrapper(t) {
            out.push(Entry { text: Some(s.to_string()), ..mk("prompt") });
        }
        return;
    }

    let Some(blocks) = content.and_then(|c| c.as_array()) else { return };
    for b in blocks {
        match b.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "text" => {
                if let Some(t) = b.get("text").and_then(|v| v.as_str()).filter(|t| !t.trim().is_empty()) {
                    // Same block type both ways; the record's role decides
                    // whether it is something asked or something said.
                    let kind = if ty == "user" { "prompt" } else { "say" };
                    out.push(Entry { text: Some(t.to_string()), ..mk(kind) });
                }
            }
            // Kept, but marked: the view collapses it by default. Extended
            // thinking is signed and often long, and it is not what the model
            // chose to say.
            "thinking" => {
                if let Some(t) = b.get("thinking").and_then(|v| v.as_str()).filter(|t| !t.trim().is_empty()) {
                    out.push(Entry { text: Some(t.to_string()), ..mk("think") });
                }
            }
            "tool_use" => {
                let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                out.push(Entry {
                    tool: Some(name.to_string()),
                    arg: headline_arg(name, b.get("input")),
                    ..mk("tool")
                });
            }
            // Tool results arrive in USER records — that is the API's shape,
            // not a quirk of the harness.
            "tool_result" => {
                let text = b.get("content").map(flatten_result).unwrap_or_default();
                out.push(Entry {
                    text: Some(truncate(&text)),
                    error: Some(b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false)),
                    ..mk("result")
                });
            }
            _ => {}
        }
    }
}

/// A slash command the CLI expanded into an XML wrapper, or the caveat block it
/// prepends to locally-run command output.
///
/// These arrive as `user` records with no meta flag set, so without this they
/// render exactly as if the person had typed them — which is how `/compact`
/// ended up looking like a prompt in the conversation view.
fn is_command_wrapper(text: &str) -> bool {
    text.starts_with("<command-name>")
        || text.starts_with("<command-message>")
        || text.starts_with("<local-command-")
        || text.starts_with("<user-prompt-submit-hook>")
}

/// The one argument worth showing beside a tool name.
///
/// Chosen per tool rather than generically: `Bash` is its command, a file tool
/// is its path. A generic "first field" rule picks something useless often
/// enough to be worse than nothing.
fn headline_arg(tool: &str, input: Option<&Value>) -> Option<String> {
    let input = input?.as_object()?;
    let pick = |k: &str| input.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let arg = match tool {
        "Bash" => pick("command"),
        "Read" | "Write" | "Edit" | "NotebookEdit" => pick("file_path"),
        "Glob" | "Grep" => pick("pattern"),
        "WebFetch" => pick("url"),
        "WebSearch" => pick("query"),
        "Task" | "Agent" => pick("description"),
        "Skill" => pick("skill"),
        _ => None,
    }
    // Anything else: fall back to a short string field if there is exactly one
    // obvious candidate, otherwise show nothing rather than something wrong.
    .or_else(|| {
        let mut strings = input.values().filter_map(|v| v.as_str());
        let first = strings.next()?;
        strings.next().is_none().then(|| first.to_string())
    })?;
    // Tool lines are one line each; a multi-line command would break the layout
    // and the first line is the identifying part anyway.
    let one_line = arg.lines().next().unwrap_or("").trim().to_string();
    // Ellipsis, not the multi-line "… truncated" marker: this is rendered inline
    // beside the tool name, where an injected newline breaks the row.
    (!one_line.is_empty()).then(|| ellipsis(&one_line, 160))
}

/// Tool result content is either a string or a block list, depending on the
/// tool.
fn flatten_result(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// Shorten to fit on one line. Used where the entry IS a line.
fn ellipsis(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

fn truncate(s: &str) -> String {
    truncate_to(s, MAX_RESULT_CHARS)
}

/// Truncate on a character boundary, and say so — a silently cut result reads
/// as a tool that returned less than it did.
fn truncate_to(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}\n… truncated")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entries(records: &[Value]) -> Vec<Entry> {
        let mut out = Vec::new();
        for r in records {
            push_entry(r, &mut out);
        }
        out
    }

    #[test]
    fn the_project_key_dashes_every_non_alphanumeric() {
        assert_eq!(project_key("/Users/me/app"), "-Users-me-app");
        assert_eq!(project_key("/Users/me/MDv0.3.0"), "-Users-me-MDv0-3-0");
    }

    /// A session id becomes a path segment, so traversal is refused rather than
    /// cleaned up.
    #[test]
    fn a_session_id_cannot_traverse() {
        let home = std::env::temp_dir();
        assert!(session_file(&home, "/w", "../../etc/passwd").is_none());
        assert!(session_file(&home, "/w", "a/b").is_none());
        assert!(session_file(&home, "/w", "").is_none());
    }

    /// The CLI writes titles, modes, queue operations and file snapshots into
    /// the same file. None of it is conversation.
    #[test]
    fn only_conversation_records_become_entries() {
        let out = entries(&[
            json!({ "type": "ai-title", "aiTitle": "x" }),
            json!({ "type": "mode", "mode": "y" }),
            json!({ "type": "file-history-snapshot" }),
            json!({ "type": "attachment" }),
            json!({ "type": "system" }),
            json!({ "type": "user", "message": { "content": "do the thing" } }),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "prompt");
        assert_eq!(out[0].text.as_deref(), Some("do the thing"));
    }

    #[test]
    fn meta_records_are_not_conversation_either() {
        let out = entries(&[
            json!({ "type": "user", "isMeta": true, "message": { "content": "injected" } }),
            json!({ "type": "user", "isVisibleInTranscriptOnly": true,
                    "message": { "content": "internal" } }),
        ]);
        assert!(out.is_empty());
    }

    #[test]
    fn assistant_text_thinking_and_tools_become_distinct_entries() {
        let out = entries(&[json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "thinking", "thinking": "weighing it up" },
                { "type": "text", "text": "Here is the plan." },
                { "type": "tool_use", "name": "Bash", "input": { "command": "ls -la\nsecond line" } },
            ]}
        })]);
        assert_eq!(out.iter().map(|e| e.kind).collect::<Vec<_>>(), ["think", "say", "tool"]);
        assert_eq!(out[2].tool.as_deref(), Some("Bash"));
        // One line per tool entry: a multi-line command would break the layout.
        assert_eq!(out[2].arg.as_deref(), Some("ls -la"));
    }

    /// Tool results arrive in USER records — the API's shape, not a quirk.
    #[test]
    fn a_tool_result_in_a_user_record_is_a_result_not_a_prompt() {
        let out = entries(&[json!({
            "type": "user",
            "message": { "content": [
                { "type": "tool_result", "tool_use_id": "t1", "content": "ok\n", "is_error": false },
            ]}
        })]);
        assert_eq!(out[0].kind, "result");
        assert_eq!(out[0].error, Some(false));
        assert_eq!(out[0].text.as_deref(), Some("ok\n"));
    }

    #[test]
    fn a_failing_tool_is_marked() {
        let out = entries(&[json!({
            "type": "user",
            "message": { "content": [
                { "type": "tool_result", "content": [{ "type": "text", "text": "boom" }],
                  "is_error": true },
            ]}
        })]);
        assert_eq!(out[0].error, Some(true));
        assert_eq!(out[0].text.as_deref(), Some("boom"));
    }

    /// The headline argument is picked per tool: a generic "first field" rule
    /// picks something useless often enough to be worse than nothing.
    #[test]
    fn each_tool_shows_its_own_identifying_argument() {
        let cases = [
            ("Read", json!({ "file_path": "/a/b.rs", "offset": 10 }), Some("/a/b.rs")),
            ("Grep", json!({ "pattern": "fn main", "path": "/x" }), Some("fn main")),
            ("WebFetch", json!({ "url": "https://x.dev", "prompt": "read it" }), Some("https://x.dev")),
            // Unknown tool, one string field → safe to show.
            ("Mystery", json!({ "only": "the one" }), Some("the one")),
            // Unknown tool, several → show nothing rather than the wrong one.
            ("Mystery", json!({ "a": "one", "b": "two" }), None),
            ("Mystery", json!({ "n": 1 }), None),
        ];
        for (tool, input, want) in cases {
            assert_eq!(headline_arg(tool, Some(&input)).as_deref(), want, "{tool} {input}");
        }
    }

    /// A tool entry is one row, so its argument must not gain a newline.
    #[test]
    fn a_long_tool_argument_stays_on_one_line() {
        let input = json!({ "command": "x".repeat(400) });
        let arg = headline_arg("Bash", Some(&input)).unwrap();
        assert!(!arg.contains('\n'), "an inline argument must not wrap");
        assert!(arg.ends_with('…'));
        assert_eq!(arg.chars().count(), 160);
    }

    /// `/compact` and friends arrive as ordinary user records with no meta flag,
    /// so without filtering they read as something the person typed.
    #[test]
    fn slash_command_machinery_is_not_a_prompt() {
        let out = entries(&[
            json!({ "type": "user", "message": { "content":
                "<command-name>/compact</command-name>\n<command-args></command-args>" }}),
            json!({ "type": "user", "message": { "content":
                "<local-command-caveat>ignore this</local-command-caveat>" }}),
            json!({ "type": "user", "message": { "content": "a real question" }}),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text.as_deref(), Some("a real question"));
    }

    #[test]
    fn long_output_is_truncated_visibly() {
        let long = "x".repeat(MAX_RESULT_CHARS + 500);
        let out = truncate(&long);
        assert!(out.ends_with("… truncated"), "a silent cut reads as a shorter result");
        assert_eq!(out.chars().count(), MAX_RESULT_CHARS + "\n… truncated".chars().count());
    }

    #[test]
    fn truncation_does_not_split_a_character() {
        assert_eq!(truncate_to("héllo wörld", 4), "héll\n… truncated");
    }

    /// The writer appends while we read, so a torn trailing record must be left
    /// for the next call rather than parsed or skipped.
    #[test]
    fn reading_stops_on_a_line_boundary() {
        let dir = std::env::temp_dir().join(format!("md-tr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");

        let complete = json!({ "type": "user", "message": { "content": "one" } }).to_string();
        std::fs::write(&path, format!("{complete}\n{{\"type\":\"assist")).unwrap();

        let page = read(&path, 0).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.cursor, complete.len() as u64 + 1, "cursor sits after the newline");

        // The writer finishes the record; the next read picks it up whole.
        let rest = json!({ "type": "assistant", "message": { "content": [
            { "type": "text", "text": "two" }]}}).to_string();
        std::fs::write(&path, format!("{complete}\n{rest}\n")).unwrap();

        let page2 = read(&path, page.cursor).unwrap();
        assert_eq!(page2.entries.len(), 1);
        assert_eq!(page2.entries[0].text.as_deref(), Some("two"));
    }

    /// A new session can write to the same path. A cursor past the end must
    /// restart rather than return nothing forever.
    #[test]
    fn a_replaced_transcript_restarts_from_the_beginning() {
        let dir = std::env::temp_dir().join(format!("md-tr2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        std::fs::write(&path, json!({ "type": "user", "message": { "content": "fresh" }}).to_string() + "\n").unwrap();

        let page = read(&path, 999_999).unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].text.as_deref(), Some("fresh"));
    }

    #[test]
    fn subagent_traffic_is_marked_so_the_view_can_fold_it() {
        let out = entries(&[json!({
            "type": "assistant", "isSidechain": true,
            "message": { "content": [{ "type": "text", "text": "sub" }] }
        })]);
        assert!(out[0].sidechain);
    }
}
