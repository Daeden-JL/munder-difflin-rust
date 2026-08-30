//! The file browser and editor.
//!
//! **A `<textarea>` with a highlighted layer behind it, not a custom editor** —
//! and not `ropey` + `tree-sitter` as the conversion plan proposed.
//!
//! A custom editor has to reimplement everything a textarea already does
//! correctly: undo/redo, IME composition for non-Latin input, screen-reader
//! support, spellcheck, native selection and drag, mobile keyboards. Getting
//! any of those subtly wrong is worse than having no syntax colours. The
//! plan's own checklist already conceded that Monaco parity was not a realistic
//! target; this takes the concession seriously rather than half-building one.
//!
//! `tree-sitter` would mean shipping a WASM grammar blob per language — several
//! hundred KB each — to colour keywords in a side panel. The tokenizer below is
//! a few hundred lines and covers the same languages the original configured.
//!
//! What is genuinely lost: no semantic highlighting, no folding, no
//! multi-cursor. Those belong to the IDE panel, which is a separate surface.

use leptos::prelude::*;
use serde_json::json;

use crate::api;

/// Files above this are shown read-only. The editor sends the whole document on
/// save, so a large file means a large request on every keystroke-triggered
/// save — and a textarea stops being pleasant long before that anyway.
const MAX_EDITABLE: usize = 512 * 1024;

/// A token class, which is also its CSS class.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Tok {
    Plain,
    Keyword,
    Str,
    Comment,
    Number,
    Punct,
}

impl Tok {
    fn class(self) -> &'static str {
        match self {
            Tok::Keyword => "t-kw",
            Tok::Str => "t-str",
            Tok::Comment => "t-com",
            Tok::Number => "t-num",
            Tok::Punct => "t-pun",
            Tok::Plain => "t-pln",
        }
    }
}

/// The languages the original configured, plus Rust — the one this codebase is
/// written in, which would be a strange omission.
fn keywords(ext: &str) -> &'static [&'static str] {
    match ext {
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => &[
            "const", "let", "var", "function", "return", "if", "else", "for", "while", "class",
            "import", "export", "from", "async", "await", "new", "try", "catch", "throw", "typeof",
            "interface", "type", "extends", "implements", "public", "private", "static", "null",
            "undefined", "true", "false",
        ],
        "rs" => &[
            "fn", "let", "mut", "const", "struct", "enum", "impl", "trait", "pub", "use", "mod",
            "match", "if", "else", "for", "while", "loop", "return", "self", "Self", "where",
            "async", "await", "move", "ref", "dyn", "crate", "super", "true", "false", "unsafe",
        ],
        "py" => &[
            "def", "class", "return", "if", "elif", "else", "for", "while", "import", "from",
            "as", "try", "except", "finally", "raise", "with", "lambda", "None", "True", "False",
            "and", "or", "not", "in", "is", "pass", "yield", "async", "await", "self",
        ],
        "html" | "htm" | "xml" => &["html", "head", "body", "div", "span", "script", "style", "link", "meta"],
        "css" | "scss" => &["color", "background", "display", "flex", "grid", "margin", "padding", "border", "font"],
        "yml" | "yaml" => &["true", "false", "null", "yes", "no", "on", "off"],
        "json" => &["true", "false", "null"],
        _ => &[],
    }
}

/// Which comment syntax a language uses. `#` languages have no block form,
/// which is why this is two separate answers rather than one.
fn comment_syntax(ext: &str) -> (Option<&'static str>, bool) {
    match ext {
        "py" | "yml" | "yaml" | "sh" | "toml" | "conf" => (Some("#"), false),
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "rs" | "css" | "scss" | "go" | "java" | "c" | "cpp" => {
            (Some("//"), true)
        }
        _ => (None, false),
    }
}

/// Split source into classified spans.
///
/// Deliberately a lexer and not a parser: it colours what a reader scans for —
/// strings, comments, keywords, numbers — and never claims to understand the
/// code. A parser that is subtly wrong looks broken; a lexer that stops at
/// tokens looks exactly like what it is.
fn tokenize(src: &str, ext: &str) -> Vec<(Tok, String)> {
    let kw = keywords(ext);
    let (line_comment, block_comments) = comment_syntax(ext);
    let chars: Vec<char> = src.chars().collect();
    let mut out: Vec<(Tok, String)> = Vec::new();
    let mut i = 0;

    let push = |out: &mut Vec<(Tok, String)>, t: Tok, s: String| {
        // Merge adjacent runs of the same class, so a paragraph of plain text is
        // one span instead of one per character.
        match out.last_mut() {
            Some((lt, ls)) if *lt == t => ls.push_str(&s),
            _ => out.push((t, s)),
        }
    };

    while i < chars.len() {
        let c = chars[i];

        // Comments.
        if let Some(marker) = line_comment {
            let m: Vec<char> = marker.chars().collect();
            if chars[i..].starts_with(&m[..]) {
                let start = i;
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                push(&mut out, Tok::Comment, chars[start..i].iter().collect());
                continue;
            }
        }
        if block_comments && c == '/' && chars.get(i + 1) == Some(&'*') {
            let start = i;
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            push(&mut out, Tok::Comment, chars[start..i].iter().collect());
            continue;
        }

        // Strings. An unterminated one runs to end of line rather than to end of
        // file, so a stray quote colours one line instead of the rest of the
        // document.
        if c == '"' || c == '\'' || c == '`' {
            let quote = c;
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != quote {
                if chars[i] == '\\' {
                    i += 1;
                }
                if chars[i.min(chars.len() - 1)] == '\n' && quote != '`' {
                    break;
                }
                i += 1;
            }
            i = (i + 1).min(chars.len());
            push(&mut out, Tok::Str, chars[start..i].iter().collect());
            continue;
        }

        // Numbers.
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_') {
                i += 1;
            }
            push(&mut out, Tok::Number, chars[start..i].iter().collect());
            continue;
        }

        // Identifiers, which may be keywords.
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let t = if kw.contains(&word.as_str()) { Tok::Keyword } else { Tok::Plain };
            push(&mut out, t, word);
            continue;
        }

        if "{}[]()<>;,.:=+-*/%&|!?".contains(c) {
            push(&mut out, Tok::Punct, c.to_string());
        } else {
            push(&mut out, Tok::Plain, c.to_string());
        }
        i += 1;
    }
    out
}

fn extension(path: &str) -> String {
    path.rsplit('/')
        .next()
        .and_then(|f| f.rsplit_once('.'))
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default()
}

#[component]
pub fn Editor(root: RwSignal<String>) -> impl IntoView {
    let entries = RwSignal::new(Vec::<(String, bool)>::new());
    let path = RwSignal::new(Option::<String>::None);
    let content = RwSignal::new(String::new());
    let original = RwSignal::new(String::new());
    let status = RwSignal::new(String::new());
    let read_only = RwSignal::new(false);

    // Browse. The directory listing is the tenant-scoped `fs:listDir`, so the
    // tree can never show a path outside the tenant's home.
    Effect::new(move |_| {
        let dir = root.get();
        leptos::task::spawn_local(async move {
            match api::rpc("fs:listDir", json!([dir, ""])).await {
                Ok(v) => {
                    let mut list: Vec<(String, bool)> = v["entries"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|e| {
                                    Some((e["name"].as_str()?.to_string(), e["isDir"].as_bool()?))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    // Directories first, then names — the order a file tree is
                    // read in.
                    list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.to_lowercase().cmp(&b.0.to_lowercase())));
                    entries.set(list);
                }
                Err(e) => status.set(e),
            }
        });
    });

    let open = move |name: String, is_dir: bool| {
        let base = root.get_untracked();
        let full = format!("{}/{}", base.trim_end_matches('/'), name);
        if is_dir {
            root.set(full);
            path.set(None);
            return;
        }
        leptos::task::spawn_local(async move {
            match api::rpc("fs:readFile", json!([base, name])).await {
                Ok(v) if v["ok"] == true => {
                    let text = v["content"].as_str().unwrap_or("").to_string();
                    read_only.set(text.len() > MAX_EDITABLE);
                    content.set(text.clone());
                    original.set(text);
                    path.set(Some(full));
                    status.set(String::new());
                }
                Ok(v) => status.set(v["error"].as_str().unwrap_or("could not read").to_string()),
                Err(e) => status.set(e),
            }
        });
    };

    let save = move |_| {
        let (Some(full), base) = (path.get_untracked(), root.get_untracked()) else { return };
        let rel = full.rsplit('/').next().unwrap_or("").to_string();
        let body = content.get_untracked();
        status.set("saving…".into());
        leptos::task::spawn_local(async move {
            match api::rpc("fs:writeFile", json!([base, rel, body.clone()])).await {
                Ok(v) if v["ok"] == true => {
                    // The saved text becomes the new baseline, so the dirty flag
                    // clears without a re-read.
                    original.set(body);
                    status.set("saved".into());
                }
                Ok(v) => status.set(v["error"].as_str().unwrap_or("save failed").to_string()),
                Err(e) => status.set(e),
            }
        });
    };

    let dirty = move || content.get() != original.get();

    view! {
        <div class="editor">
            <div class="ed-bar">
                <button class="ghost" on:click=move |_| {
                    // Up one level, but never above the tenant home — the server
                    // would refuse it anyway, and a dead-end button is worse
                    // than one that stops.
                    let cur = root.get();
                    if let Some((parent, _)) = cur.trim_end_matches('/').rsplit_once('/') {
                        if !parent.is_empty() { root.set(parent.to_string()); }
                    }
                }>"↑"</button>
                <span class="path">{move || root.get()}</span>
                <Show when=move || path.get().is_some()>
                    <button on:click=save prop:disabled=move || !dirty() || read_only.get()>
                        {move || if dirty() { "save" } else { "saved" }}
                    </button>
                </Show>
                <span class="dim">{move || status.get()}</span>
            </div>
            <div class="ed-body">
                <div class="tree">
                    <For each=move || entries.get() key=|e| e.0.clone() let:entry>
                        {
                            let (name, is_dir) = entry;
                            let label = if is_dir { format!("{name}/") } else { name.clone() };
                            view! {
                                <button class="row" class:dir=is_dir
                                        on:click=move |_| open(name.clone(), is_dir)>
                                    {label}
                                </button>
                            }
                        }
                    </For>
                </div>
                <div class="pane">
                    <Show when=move || path.get().is_some() fallback=|| view! {
                        <p class="empty">"Pick a file."</p>
                    }>
                        <Show when=move || read_only.get()>
                            <p class="warn">"This file is too large to edit here — shown read-only."</p>
                        </Show>
                        // The highlighted layer sits BEHIND the textarea, which
                        // is transparent-texted. They share a font and metrics,
                        // so the colours line up with the characters the user is
                        // actually editing.
                        <div class="code">
                            <pre class="hl" aria-hidden="true">
                                {move || {
                                    let ext = path.get().map(|p| extension(&p)).unwrap_or_default();
                                    tokenize(&content.get(), &ext)
                                        .into_iter()
                                        .map(|(t, s)| view! { <span class=t.class()>{s}</span> })
                                        .collect::<Vec<_>>()
                                }}
                            </pre>
                            <textarea
                                spellcheck="false"
                                prop:value=move || content.get()
                                prop:readonly=move || read_only.get()
                                on:input=move |e| content.set(event_target_value(&e))
                            />
                        </div>
                    </Show>
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classes(src: &str, ext: &str) -> Vec<(Tok, String)> {
        tokenize(src, ext)
    }

    fn has(src: &str, ext: &str, t: Tok, text: &str) -> bool {
        classes(src, ext).iter().any(|(k, s)| *k == t && s.contains(text))
    }

    #[test]
    fn extensions_are_read_from_the_filename_not_the_path() {
        assert_eq!(extension("a/b/c.rs"), "rs");
        assert_eq!(extension("a.dir/file"), "");
        assert_eq!(extension("UPPER.TS"), "ts");
        assert_eq!(extension("noext"), "");
    }

    #[test]
    fn keywords_are_per_language() {
        assert!(has("fn main() {}", "rs", Tok::Keyword, "fn"));
        // `fn` is not a keyword in Python, and colouring it would be a lie.
        assert!(!has("fn main()", "py", Tok::Keyword, "fn"));
        assert!(has("def main():", "py", Tok::Keyword, "def"));
        assert!(has("const x = 1", "ts", Tok::Keyword, "const"));
    }

    #[test]
    fn strings_comments_and_numbers_are_classified() {
        assert!(has(r#"let s = "hello";"#, "rs", Tok::Str, "\"hello\""));
        assert!(has("// a note\ncode", "rs", Tok::Comment, "// a note"));
        assert!(has("# a note", "py", Tok::Comment, "# a note"));
        assert!(has("/* block */ x", "rs", Tok::Comment, "/* block */"));
        assert!(has("x = 42", "rs", Tok::Number, "42"));
    }

    /// A `#` language has no block comment form; treating `/*` as one there
    /// would grey out working code.
    #[test]
    fn a_hash_language_has_no_block_comments() {
        assert!(!has("x = 1 /* not a comment */", "py", Tok::Comment, "/*"));
    }

    /// A stray quote should colour one line, not the rest of the file.
    #[test]
    fn an_unterminated_string_stops_at_the_line() {
        let out = classes("let a = \"oops\nlet b = 2;\n", "rs");
        let strings: String = out.iter().filter(|(t, _)| *t == Tok::Str).map(|(_, s)| s.clone()).collect();
        assert!(!strings.contains("let b"), "the string swallowed the next line");
    }

    /// Every character of the input must survive tokenizing, or the highlighted
    /// layer drifts out of alignment with the textarea behind it.
    #[test]
    fn tokenizing_is_lossless() {
        for (src, ext) in [
            ("fn main() { let x = \"hi\"; /* c */ }\n// end\n", "rs"),
            ("def f():\n  # note\n  return 'x'\n", "py"),
            ("{\"a\": 1, \"b\": [true, null]}", "json"),
            ("<div class=\"x\">text</div>", "html"),
            ("", "rs"),
            ("plain text with no code", "txt"),
            ("émoji 🙂 and ünicode", "rs"),
        ] {
            let joined: String = tokenize(src, ext).into_iter().map(|(_, s)| s).collect();
            assert_eq!(joined, src, "lost characters in {ext}");
        }
    }

    /// Adjacent same-class runs merge, or a paragraph becomes one span per
    /// character and the DOM balloons.
    #[test]
    fn adjacent_runs_of_the_same_class_merge() {
        let out = tokenize("plain words here", "txt");
        assert_eq!(out.len(), 1, "expected one merged span, got {}", out.len());
        assert_eq!(out[0].0, Tok::Plain);
    }
}
