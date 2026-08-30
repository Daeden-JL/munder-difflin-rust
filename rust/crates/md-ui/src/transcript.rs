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
pub fn Conversation(agent: RwSignal<Option<String>>, activity: RwSignal<u32>) -> impl IntoView {
    // Carries an explicit sequence number: entries are append-only and never
    // change, so the index is a stable key and the list never re-renders what
    // is already on screen.
    let entries = RwSignal::new(Vec::<(usize, Entry)>::new());
    let cursor = RwSignal::new(0u64);
    let waiting = RwSignal::new(false);
    let show_thinking = RwSignal::new(false);
    let sending = RwSignal::new(false);
    let error = RwSignal::new(String::new());

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
        sending.set(true);
        leptos::task::spawn_local(async move {
            // A carriage return submits it, the same keystroke a person at the
            // terminal would send.
            let payload = json!([id, format!("{}\r", text)]);
            match api::rpc("pty:write", payload).await {
                Ok(_) => error.set(String::new()),
                Err(e) => error.set(e),
            }
            sending.set(false);
        });
    };

    view! {
        <main class="convo">
            <div class="stream">
                <Show when=move || agent.get().is_none()>
                    <p class="empty">"Pick an agent."</p>
                </Show>
                <Show when=move || waiting.get() && entries.get().is_empty()>
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
        _ => view! { <div class="text">{e.text.clone().unwrap_or_default()}</div> }.into_any(),
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
