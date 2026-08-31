//! The conversation view and the composer.
//!
//! This is what replaces the terminal. Entries come from the agent's own
//! session transcript — real tool names, real arguments, real results — rather
//! than from parsing escape sequences off a screen.
//!
//! The composer types into the agent's PTY, because that is how you talk to a
//! CLI. The PTY is still the transport; it is simply never rendered.

use leptos::prelude::*;
use serde::Deserialize;
use serde_json::json;

use crate::api;

/// How often the transcript is polled while an agent is working. The socket
/// tells us *that* something happened; the transcript is the file the CLI
/// appends to, so it still has to be read.
const FOLLOW_MS: u32 = 1_200;

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub arg: Option<String>,
    #[serde(default)]
    pub error: Option<bool>,
    #[serde(default)]
    pub sidechain: bool,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Page {
    #[serde(default)]
    entries: Vec<Entry>,
    #[serde(default)]
    cursor: u64,
    #[serde(default)]
    more: bool,
    /// The agent has not reported a session yet — nothing to show, keep polling.
    #[serde(default)]
    waiting: bool,
}

#[component]
pub fn Conversation(
    agent: RwSignal<Option<String>>,
    activity: RwSignal<u32>,
    /// Who the selected agent is dressed as, so the header can say something
    /// about them. Personality you can read, not only overhear on the floor.
    persona: Signal<Option<(String, String)>>,
    /// Whether the selected agent has a process. A message goes to the agent's
    /// terminal, so with no terminal there is nowhere for it to go — and that
    /// is worth saying rather than discovering.
    live: Signal<bool>,
    /// Start the selected agent again.
    on_start: Callback<()>,
) -> impl IntoView {
    // Carries an explicit sequence number: entries are append-only and never
    // change, so the index is a stable key and the list never re-renders what
    // is already on screen.
    let entries = RwSignal::new(Vec::<(usize, Entry)>::new());
    let cursor = RwSignal::new(0u64);
    let waiting = RwSignal::new(false);
    let show_thinking = RwSignal::new(false);
    let sending = RwSignal::new(false);
    let error = RwSignal::new(String::new());
    let stream_ref = NodeRef::<leptos::html::Div>::new();
    // Follow the tail — but stop following the moment the reader scrolls up, or
    // the view would yank them back to the bottom mid-sentence.
    let pinned = RwSignal::new(true);

    // Switching agents starts a new conversation, so the accumulated entries and
    // the cursor must both reset — keeping the cursor would silently show the
    // new agent only what arrives from here on.
    Effect::new(move |_| {
        let _ = agent.get();
        entries.set(vec![]);
        cursor.set(0);
        error.set(String::new());
    });

    // Follow the transcript. Reads from the cursor, so each poll transfers only
    // what was appended, and a long session does not get more expensive.
    Effect::new(move |_| {
        let _ = (agent.get(), activity.get());
        let Some(id) = agent.get() else { return };
        leptos::task::spawn_local(async move {
            loop {
                let at = cursor.get_untracked();
                let url = format!("/api/transcript?agent={id}&cursor={at}");
                match api::get_json(&url).await {
                    Ok(v) => {
                        let page: Page = serde_json::from_value(v).unwrap_or_default();
                        waiting.set(page.waiting);
                        if !page.entries.is_empty() {
                            let fresh = page.entries;
                            entries.update(|e| {
                                for (n, entry) in (e.len()..).zip(fresh) {
                                    e.push((n, entry));
                                }
                            });
                            cursor.set(page.cursor);
                        }
                        // `more` means the read hit its cap, so the rest is
                        // already waiting — do not sleep before fetching it.
                        if page.more {
                            continue;
                        }
                    }
                    Err(e) => error.set(e),
                }
                gloo_timers::future::TimeoutFuture::new(FOLLOW_MS).await;
                // Stop this loop when the selection moves on, so switching
                // agents does not leave a poller running per agent visited.
                if agent.get_untracked().as_deref() != Some(id.as_str()) {
                    return;
                }
            }
        });
    });

    let send = move |text: String| {
        let Some(id) = agent.get_untracked() else { return };
        if text.trim().is_empty() {
            return;
        }
        // Said before the attempt rather than after it. A message to an agent
        // with no process cannot go anywhere, and the failure it produced was
        // in-band and therefore invisible — which reads as the chat being
        // broken rather than the agent being stopped.
        if !live.get_untracked() {
            error.set("This agent is not running. Start it, then send again.".into());
            return;
        }
        sending.set(true);
        leptos::task::spawn_local(async move {
            // A carriage return submits it, the same keystroke a person at the
            // terminal would send.
            let payload = json!([id, format!("{}\r", text)]);
            match api::rpc("pty:write", payload).await {
                // `pty:write` reports failure IN BAND — `{ok:false, error}`
                // inside a successful envelope — so treating any `Ok` as
                // success swallowed every one of them and cleared the error
                // line for good measure.
                Ok(v) if v["ok"] == false => error.set(
                    v["error"].as_str().unwrap_or("the message could not be delivered").to_string(),
                ),
                Ok(_) => error.set(String::new()),
                Err(e) => error.set(e),
            }
            sending.set(false);
        });
    };

    // Runs after each batch of entries is appended.
    Effect::new(move |_| {
        let _ = entries.get();
        if !pinned.get_untracked() {
            return;
        }
        if let Some(el) = stream_ref.get_untracked() {
            el.set_scroll_top(el.scroll_height());
        }
    });

    let on_scroll = move |_| {
        if let Some(el) = stream_ref.get_untracked() {
            // A small slack, so being a pixel off the bottom still counts as
            // following.
            let at_bottom = el.scroll_height() - el.scroll_top() - el.client_height() < 40;
            pinned.set(at_bottom);
        }
    };

    view! {
        <main class="convo">
            <Show when=move || persona.get().is_some()>
                <div class="persona">
                    <b>{move || persona.get().map(|p| p.0).unwrap_or_default()}</b>
                    <i>{move || persona.get().map(|p| p.1).unwrap_or_default()}</i>
                </div>
            </Show>
            <div class="stream" node_ref=stream_ref on:scroll=on_scroll>
                <Show when=move || agent.get().is_none()>
                    <p class="empty">"Pick an agent."</p>
                </Show>
                // Three different empty states, and they used to be one. An
                // agent with no process is not an agent that has yet to speak.
                <Show when=move || agent.get().is_some() && !live.get()>
                    <p class="empty">
                        "This agent is not running. Everything it remembers is kept — start it
                         and it picks up where it left off."
                        <button on:click=move |_| on_start.run(())>"start this agent"</button>
                    </p>
                </Show>
                <Show when=move || live.get() && waiting.get() && entries.get().is_empty()>
                    <p class="empty">
                        "Waiting for this agent to start a session. Type a task below to begin."
                    </p>
                </Show>
                <For each=move || entries.get() key=|(i, _)| *i let:pair>
                    {
                        let e = pair.1;
                        // Extended thinking is signed, long, and not what the
                        // model chose to say — kept, but folded by default.
                        let hidden = e.kind == "think";
                        view! {
                            <Show when=move || !hidden || show_thinking.get()>
                                {render(&e)}
                            </Show>
                        }
                    }
                </For>
            </div>

            <div class="compose">
                <p class="err">{move || error.get()}</p>
                // Shown only when there is thinking to show. Real transcripts
                // persist the signature but not the text — every thinking block
                // in a live session file is empty — so an unconditional toggle
                // would be a control that provably does nothing. The parser
                // still handles it, for a CLI version that does persist it.
                <Show when=move || entries.get().iter().any(|(_, e)| e.kind == "think")>
                    <div class="row">
                        <label class="toggle">
                            <input type="checkbox" prop:checked=move || show_thinking.get()
                                   on:change=move |e| show_thinking.set(event_target_checked(&e))/>
                            "show thinking"
                        </label>
                    </div>
                </Show>
                <Composer send=send disabled=sending/>
            </div>
        </main>
    }
}

/// One entry. Tool calls and their results are deliberately compact — the point
/// of this view over a terminal is that a turn is skimmable.
fn render(e: &Entry) -> impl IntoView {
    let class = format!("e {}{}", e.kind, if e.sidechain { " sub" } else { "" });
    let body = match e.kind.as_str() {
        "tool" => {
            let name = e.tool.clone().unwrap_or_default();
            let arg = e.arg.clone().unwrap_or_default();
            view! {
                <span class="tool"><b>{name}</b> <code>{arg}</code></span>
            }
            .into_any()
        }
        "result" => {
            let failed = e.error.unwrap_or(false);
            let text = e.text.clone().unwrap_or_default();
            view! { <pre class="result" class:failed=failed>{text}</pre> }.into_any()
        }
        // Prose is markdown — agents write headings, fences and tables, and
        // raw syntax competes with the content. Tool RESULTS are deliberately
        // not markdown: they are program output, where whitespace is meaning.
        _ => {
            let text = e.text.clone().unwrap_or_default();
            view! { <div class="text">{crate::markdown::render(&text)}</div> }.into_any()
        }
    };
    view! { <div class=class>{body}</div> }
}

/// Enter sends, Shift+Enter adds a line — the convention every chat surface
/// uses, and the one people try first.
#[component]
fn Composer(send: impl Fn(String) + 'static + Copy, disabled: RwSignal<bool>) -> impl IntoView {
    let text = RwSignal::new(String::new());

    let submit = move || {
        send(text.get_untracked());
        text.set(String::new());
    };

    view! {
        <div class="composer">
            <textarea
                rows="3"
                placeholder="Describe a task…"
                prop:value=move || text.get()
                prop:disabled=move || disabled.get()
                on:input=move |e| text.set(event_target_value(&e))
                on:keydown=move |e: leptos::ev::KeyboardEvent| {
                    if e.key() == "Enter" && !e.shift_key() {
                        e.prevent_default();
                        submit();
                    }
                }
            />
            <button prop:disabled=move || disabled.get() on:click=move |_| submit()>
                {move || if disabled.get() { "sending…" } else { "send" }}
            </button>
        </div>
    }
}
