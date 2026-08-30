//! Just enough markdown to make agent output readable.
//!
//! Agents write markdown — headings, fenced code, tables, bold — and shown raw
//! it is worse than plain prose, because the syntax competes with the content.
//!
//! **Rendered as nodes, never as HTML.** `pulldown-cmark` can emit an HTML
//! string that would be dropped in with `inner_html`, and that would make every
//! tool result a script injection vector: an agent reads a file from a
//! repository, the contents land in the transcript, and the page executes them.
//! So HTML events are parsed and discarded, and the subset below is built as
//! real elements.

use leptos::prelude::*;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// One rendered chunk. A flat list rather than a tree: the subset here nests
/// only inside list items and table cells, both of which are handled by
/// gathering their inline text.
enum Node {
    Para(Vec<Span>),
    Heading(u8, String),
    Code { lang: String, text: String },
    Item(Vec<Span>),
    Quote(String),
    Rule,
    /// A table is shown as its rows, tab-separated. Real table layout is not
    /// worth the code here; keeping the cells aligned in a monospace block
    /// carries the same information.
    Table(Vec<String>),
}

/// Inline formatting inside a paragraph.
#[derive(Clone)]
enum Span {
    Text(String),
    Strong(String),
    Em(String),
    Code(String),
    /// The URL is shown, not linked: a clickable link built from model output
    /// is a phishing surface, and the text is what the reader needs anyway.
    Link { text: String, href: String },
}

pub fn render(src: &str) -> impl IntoView {
    let nodes = parse(src);
    view! {
        <div class="md">
            {nodes.into_iter().map(render_node).collect::<Vec<_>>()}
        </div>
    }
}

fn render_node(n: Node) -> AnyView {
    match n {
        Node::Para(spans) => view! { <p>{spans.into_iter().map(render_span).collect::<Vec<_>>()}</p> }.into_any(),
        Node::Heading(level, text) => {
            let class = format!("h h{level}");
            view! { <div class=class>{text}</div> }.into_any()
        }
        Node::Code { lang, text } => view! {
            <pre class="code"><span class="lang">{lang}</span><code>{text}</code></pre>
        }
        .into_any(),
        Node::Item(spans) => view! {
            <div class="li">"• "{spans.into_iter().map(render_span).collect::<Vec<_>>()}</div>
        }
        .into_any(),
        Node::Quote(text) => view! { <div class="quote">{text}</div> }.into_any(),
        Node::Rule => view! { <hr/> }.into_any(),
        Node::Table(rows) => view! { <pre class="table">{rows.join("\n")}</pre> }.into_any(),
    }
}

fn render_span(s: Span) -> AnyView {
    match s {
        Span::Text(t) => view! { {t} }.into_any(),
        Span::Strong(t) => view! { <b>{t}</b> }.into_any(),
        Span::Em(t) => view! { <i>{t}</i> }.into_any(),
        Span::Code(t) => view! { <code>{t}</code> }.into_any(),
        Span::Link { text, href } => {
            view! { <span class="link">{text}" ("{href}")"</span> }.into_any()
        }
    }
}

fn parse(src: &str) -> Vec<Node> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);

    let mut out = Vec::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut buf = String::new();
    // Which inline decoration the current text run belongs to.
    let mut style: Option<&'static str> = None;
    let mut link: Option<String> = None;
    let mut code: Option<(String, String)> = None;
    let mut heading: Option<u8> = None;
    let mut quote = false;
    let mut cells: Vec<String> = Vec::new();
    let mut rows: Vec<String> = Vec::new();
    let mut in_table = false;

    let flush = |buf: &mut String, spans: &mut Vec<Span>, style: &mut Option<&'static str>, link: &mut Option<String>| {
        if buf.is_empty() {
            return;
        }
        let text = std::mem::take(buf);
        spans.push(match (*style, link.take()) {
            (_, Some(href)) => Span::Link { text, href },
            (Some("strong"), _) => Span::Strong(text),
            (Some("em"), _) => Span::Em(text),
            (Some("code"), _) => Span::Code(text),
            _ => Span::Text(text),
        });
        *style = None;
    };

    for ev in Parser::new_ext(src, opts) {
        match ev {
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((lang, String::new()));
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, text)) = code.take() {
                    out.push(Node::Code { lang, text: text.trim_end().to_string() });
                }
            }
            Event::Start(Tag::Heading { level, .. }) => {
                heading = Some(match level {
                    HeadingLevel::H1 => 1,
                    HeadingLevel::H2 => 2,
                    _ => 3,
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(l) = heading.take() {
                    out.push(Node::Heading(l, std::mem::take(&mut buf)));
                }
            }
            Event::Start(Tag::BlockQuote(_)) => quote = true,
            Event::End(TagEnd::BlockQuote(_)) => {
                quote = false;
                let t = std::mem::take(&mut buf);
                if !t.trim().is_empty() {
                    out.push(Node::Quote(t));
                }
            }
            Event::Start(Tag::Table(_)) => {
                in_table = true;
                rows.clear();
            }
            Event::End(TagEnd::Table) => {
                in_table = false;
                out.push(Node::Table(std::mem::take(&mut rows)));
            }
            Event::End(TagEnd::TableRow) | Event::End(TagEnd::TableHead) => {
                rows.push(std::mem::take(&mut cells).join("  │  "));
            }
            Event::End(TagEnd::TableCell) => cells.push(std::mem::take(&mut buf)),
            Event::Start(Tag::Strong) => {
                flush(&mut buf, &mut spans, &mut style, &mut link);
                style = Some("strong");
            }
            Event::Start(Tag::Emphasis) => {
                flush(&mut buf, &mut spans, &mut style, &mut link);
                style = Some("em");
            }
            Event::End(TagEnd::Strong | TagEnd::Emphasis) => {
                flush(&mut buf, &mut spans, &mut style, &mut link)
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                flush(&mut buf, &mut spans, &mut style, &mut link);
                link = Some(dest_url.to_string());
            }
            Event::End(TagEnd::Link) => flush(&mut buf, &mut spans, &mut style, &mut link),
            Event::End(TagEnd::Item) => {
                flush(&mut buf, &mut spans, &mut style, &mut link);
                out.push(Node::Item(std::mem::take(&mut spans)));
            }
            Event::End(TagEnd::Paragraph) => {
                flush(&mut buf, &mut spans, &mut style, &mut link);
                if !spans.is_empty() {
                    out.push(Node::Para(std::mem::take(&mut spans)));
                }
            }
            Event::Code(t) => {
                flush(&mut buf, &mut spans, &mut style, &mut link);
                spans.push(Span::Code(t.to_string()));
            }
            Event::Text(t) => {
                if let Some((_, body)) = code.as_mut() {
                    body.push_str(&t);
                } else {
                    buf.push_str(&t);
                }
            }
            Event::SoftBreak => buf.push(' '),
            Event::HardBreak => buf.push('\n'),
            Event::Rule => out.push(Node::Rule),
            // Raw HTML is DISCARDED, not rendered. Agent output routinely
            // contains file contents; rendering them as markup would execute
            // whatever a repository happened to hold.
            Event::Html(_) | Event::InlineHtml(_) => {}
            _ => {}
        }
    }

    // A trailing run with no closing event (streamed, still-being-written text).
    flush(&mut buf, &mut spans, &mut style, &mut link);
    if !spans.is_empty() {
        out.push(Node::Para(spans));
    }
    if quote && !buf.is_empty() {
        out.push(Node::Quote(buf));
    }
    let _ = in_table;
    out
}
