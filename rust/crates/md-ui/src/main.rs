//! The Munder Difflin web client.
//!
//! A composer and a conversation view, not a terminal. Agent output is read
//! from the CLI's own session transcript plus live hook events, so the view
//! shows real tool names and arguments — where the Electron renderer inferred
//! them by regexing the TUI's glyphs off the screen.

mod api;
mod editor;
mod floor;
mod markdown;
mod pixel;
mod theme;
mod transcript;

use leptos::prelude::*;
use serde_json::{json, Value};

use editor::Editor;
use floor::{Floor, Occupant};
use transcript::Conversation;

/// How often the floor is re-read. Agent state changes arrive as hook events on
/// the socket; this is the slow backstop that also catches spawns and archives
/// made from elsewhere.
const ROSTER_POLL_MS: u32 = 4_000;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[derive(Clone, Debug, PartialEq)]
struct Agent {
    id: String,
    name: String,
    role: String,
    is_god: bool,
    archived: bool,
    on_hold: bool,
    /// Present once the agent's CLI has reported a session.
    has_session: bool,
    live: bool,
}

#[component]
fn App() -> impl IntoView {
    let authed = RwSignal::new(false);
    let connected = RwSignal::new(false);
    let agents = RwSignal::new(Vec::<Agent>::new());
    let selected = RwSignal::new(Option::<String>::None);
    // Bumped by any hook event, so the conversation refreshes the moment the
    // agent does something rather than on the poll interval.
    let activity = RwSignal::new(0u32);
    // The last (agentId, tool) seen on the hook stream. The floor reads it to
    // send that agent to the station the tool belongs to, which is what makes
    // the room a live picture of the fleet rather than a decoration.
    let tool_activity = RwSignal::new(Option::<(String, String)>::None);
    let status = RwSignal::new(String::new());
    // Which bundled theme is on the floor. The chosen theme changes who the
    // agents look like and nothing else — identity, memory and desk order are
    // bound to the archetype, not the character.
    let theme = RwSignal::new(0usize);
    // Which pane fills the main area. The floor and the conversation are the
    // product; files are a tool alongside them.
    let pane = RwSignal::new("chat".to_string());
    let root = RwSignal::new("~".to_string());
    // Derived ONCE and shared: the roster and the floor must not compute this
    // separately, or a face beside a name stops being the figure on the floor.
    let archetypes = Signal::derive(move || {
        let list = agents.get();
        let ids: Vec<String> = list.iter().map(|a| a.id.clone()).collect();
        let god = list.iter().find(|a| a.is_god).map(|a| a.id.clone());
        theme::assign(&ids, god.as_deref())
    });

    // A successful authenticated call is the session probe: a cookie from an
    // earlier visit is still good, so a reload does not sign you out.
    leptos::task::spawn_local(async move {
        authed.set(api::rpc("app:info", json!([])).await.is_ok());
    });

    Effect::new(move |_| {
        if !authed.get() {
            return;
        }
        api::connect(
            move |ev| {
                let channel = ev.get("channel").and_then(|c| c.as_str()).unwrap_or("");
                match channel {
                    c if c.starts_with("hive:hookEvent") => {
                        activity.update(|n| *n = n.wrapping_add(1));
                        let who = ev.pointer("/payload/agentId").and_then(|a| a.as_str()).unwrap_or("");
                        if let Some(tool) = ev.pointer("/payload/tool").and_then(|t| t.as_str()) {
                            status.set(format!("{who} · {tool}"));
                            if !who.is_empty() {
                                tool_activity.set(Some((who.to_string(), tool.to_string())));
                            }
                        }
                    }
                    // Any of these changes the roster, so re-read it rather than
                    // waiting out the poll.
                    c if c.starts_with("hive:agentSpawned") || c.starts_with("hive:agentArchived") => {
                        activity.update(|n| *n = n.wrapping_add(1));
                    }
                    _ => {}
                }
            },
            move |up| connected.set(up),
        );
    });

    // The roster: the hive registry for identity, `pty:list` for liveness. The
    // registry alone would show agents that died with a crash as if they were
    // running.
    let roster = LocalResource::new(move || {
        let _ = (activity.get(), authed.get());
        async move {
            let reg = api::rpc("hive:registry", json!([])).await.unwrap_or(Value::Null);
            let live = api::rpc("pty:list", json!([])).await.unwrap_or(Value::Null);
            let live_ids: Vec<String> = live
                .as_array()
                .map(|a| a.iter().filter_map(|s| s["id"].as_str().map(String::from)).collect())
                .unwrap_or_default();
            let mut out: Vec<Agent> = reg["agents"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(id, a)| Agent {
                            id: id.clone(),
                            name: a["name"].as_str().unwrap_or(id).to_string(),
                            role: a["role"].as_str().unwrap_or("").to_string(),
                            is_god: a["isGod"].as_bool().unwrap_or(false),
                            archived: a["archived"].as_bool().unwrap_or(false),
                            on_hold: a["onHold"].as_bool().unwrap_or(false),
                            has_session: a["sessionId"].as_str().is_some_and(|s| !s.is_empty()),
                            live: live_ids.contains(id),
                        })
                        .collect()
                })
                .unwrap_or_default();
            // The orchestrator first, then live agents, then the rest — the
            // order someone scanning the floor wants.
            out.sort_by_key(|a| (!a.is_god, !a.live, a.name.to_lowercase()));
            out
        }
    });

    Effect::new(move |_| {
        if let Some(list) = roster.get().as_deref().cloned() {
            if selected.get_untracked().is_none() {
                selected.set(list.iter().find(|a| a.live).or_else(|| list.first()).map(|a| a.id.clone()));
            }
            agents.set(list);
        }
    });

    // Slow backstop for changes no event announces.
    Effect::new(move |_| {
        if !authed.get() {
            return;
        }
        leptos::task::spawn_local(async move {
            loop {
                gloo_timers::future::TimeoutFuture::new(ROSTER_POLL_MS).await;
                activity.update(|n| *n = n.wrapping_add(1));
            }
        });
    });

    view! {
        <Show when=move || authed.get() fallback=move || view! { <Login authed/> }>
            <div class="app">
                <header>
                    <span class="brand">"munder difflin"</span>
                    <span class="dot" class:on=move || connected.get()></span>
                    <span class="status">{move || status.get()}</span>
                    <button class="ghost" class:on=move || pane.get() == "chat"
                            on:click=move |_| pane.set("chat".into())>"chat"</button>
                    <button class="ghost" class:on=move || pane.get() == "files"
                            on:click=move |_| pane.set("files".into())>"files"</button>
                    <button class="ghost" on:click=move |_| {
                        leptos::task::spawn_local(async move {
                            let _ = api::rpc("app:startClosingTime", json!([])).await;
                        });
                    }>"closing time"</button>
                </header>
                <div class="body">
                    <Roster agents selected theme slots=archetypes/>
                    <div class="main">
                        <Floor occupants=Signal::derive(move || occupants(agents.get(), &archetypes.get()))
                               theme selected activity=tool_activity archetypes/>
                        <Show
                            when=move || pane.get() == "chat"
                            fallback=move || view! { <Editor root/> }
                        >
                            <Conversation agent=selected activity persona=Signal::derive(move || {
                                let id = selected.get()?;
                                let themes = theme::builtin();
                                let t = themes.get(theme.get() % themes.len().max(1))?;
                                let arch = archetypes.get().get(&id)?.clone();
                                Some((
                                    floor::character_name(t, &arch)?,
                                    floor::character_trait(t, &arch).unwrap_or_default(),
                                ))
                            })/>
                        </Show>
                    </div>
                </div>
            </div>
        </Show>
    }
}

/// Agents as the floor draws them.
///
/// Archetypes are assigned over the roster in a STABLE order — id, not display
/// name — so renaming an agent does not reshuffle the floor, and neither does a
/// re-sort of the sidebar.
fn occupants(mut agents: Vec<Agent>, slots: &std::collections::HashMap<String, String>) -> Vec<Occupant> {
    agents.sort_by(|a, b| a.id.cmp(&b.id));
    agents
        .into_iter()
        .filter(|a| !a.archived)
        .map(|a| Occupant {
            archetype: slots.get(&a.id).cloned().unwrap_or_else(|| "leader".into()),
            status: if a.on_hold { "waiting".into() } else if a.live { "working".into() } else { "idle".into() },
            id: a.id,
            name: a.name,
            live: a.live,
        })
        .collect()
}

#[component]
fn Login(authed: RwSignal<bool>) -> impl IntoView {
    let user = RwSignal::new(String::from("dev"));
    let password = RwSignal::new(String::new());
    let error = RwSignal::new(String::new());

    let submit = move |ev: leptos::ev::SubmitEvent| {
        ev.prevent_default();
        let (u, p) = (user.get(), password.get());
        leptos::task::spawn_local(async move {
            match api::login(&u, &p).await {
                Ok(()) => authed.set(true),
                Err(e) => error.set(e),
            }
        });
    };

    view! {
        <form class="login" on:submit=submit>
            <h1>"munder difflin"</h1>
            <label>"user"</label>
            <input prop:value=move || user.get()
                   on:input=move |e| user.set(event_target_value(&e))/>
            <label>"password"</label>
            <input type="password" prop:value=move || password.get()
                   on:input=move |e| password.set(event_target_value(&e))/>
            <button type="submit">"sign in"</button>
            <p class="err">{move || error.get()}</p>
        </form>
    }
}

#[component]
fn Roster(
    agents: RwSignal<Vec<Agent>>,
    selected: RwSignal<Option<String>>,
    theme: RwSignal<usize>,
    /// The shared assignment, so the face beside a name is the figure on the
    /// floor rather than a different member of the cast.
    slots: Signal<std::collections::HashMap<String, String>>,
) -> impl IntoView {

    view! {
        <aside class="roster">
            <Show when=move || agents.get().is_empty()>
                <p class="empty">"No agents on the floor yet."</p>
            </Show>
            <For each=move || agents.get() key=|a| a.id.clone() let:a>
                {
                    let id = a.id.clone();
                    let is_selected = {
                        let id = id.clone();
                        move || selected.get().as_deref() == Some(id.as_str())
                    };
                    let face = {
                        let id = a.id.clone();
                        move || {
                            let themes = theme::builtin();
                            let t = themes.get(theme.get() % themes.len().max(1))?;
                            let arch = slots.get().get(&id)?.clone();
                            floor::portrait_data_url(t, &arch)
                        }
                    };
                    view! {
                        <button class="agent" class:sel=is_selected
                                on:click=move |_| selected.set(Some(id.clone()))>
                            <span class="pip" class:live=a.live class:hold=a.on_hold></span>
                            <img class="face" src=move || face().unwrap_or_default() alt=""/>
                            <span class="who">
                                <b>{
                                    let id = a.id.clone();
                                    let own = a.name.clone();
                                    move || {
                                        let themes = theme::builtin();
                                        themes.get(theme.get() % themes.len().max(1))
                                            .and_then(|t| slots.get().get(&id).and_then(|arch| floor::character_name(t, arch)))
                                            .unwrap_or_else(|| own.clone())
                                    }
                                }</b>
                                // Role, and who they are DRESSED as. Without the
                                // second half a theme switch changes the art and
                                // nothing says why.
                                // The agent's OWN name underneath, so the
                                // mapping is never a mystery — you can see that
                                // Mal is michael without switching themes back.
                                <i>{
                                    let own = a.name.clone();
                                    let role = if a.is_god { "orchestrator".to_string() } else { a.role.clone() };
                                    if role.is_empty() { own } else { format!("{own} · {role}") }
                                }</i>
                            </span>
                            <Show when=move || a.archived>
                                <span class="tag">"archived"</span>
                            </Show>
                        </button>
                    }
                }
            </For>
        </aside>
    }
}
